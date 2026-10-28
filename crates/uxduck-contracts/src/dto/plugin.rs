//! Plugins on the wire (ADR-0006): the installed plugin as an entity, and
//! the plugin ABI v2 (`abi: 2` in `plugin.json`). Every plugin function
//! takes and returns JSON in these shapes; the host and the SDK use the
//! same types. A plugin reads telemetry items by name, MAVLink messages
//! decoded by their own names, or raw frames, and sends commands, messages
//! by name, items with units and notifications (X1, X3, X4, X10).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::dto::command::{Command, Risk};
use crate::dto::initialize::Capability;
use crate::dto::notification::Severity;
use crate::dto::telemetry::Unit;

/// The ABI this daemon runs.
pub const PLUGIN_ABI: u32 = 2;

/// `plugin.json`, beside the plugin's WebAssembly.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginManifest {
    /// The plugin ABI it is built for: 2.
    #[serde(default = "abi_one")]
    pub abi: u32,
    /// Lower case, digits and dashes, such as `camera-manager`.
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The module, relative to the manifest, such as `camera_manager.wasm`;
    /// empty for a script.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub wasm: String,
    /// A TypeScript or JavaScript file instead of a module, such as
    /// `index.ts` (X7): UXDuck strips its types and runs it in the same
    /// sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// What it may do without asking; confirmed by the operator at install.
    pub capabilities: Vec<Capability>,
    /// What it receives, at three levels, each at its own cadence (X1).
    #[serde(default)]
    pub subscribe: Subscriptions,
    /// The MAVLink messages it may send, by name (X4). Any other is refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sends: Vec<String>,
    /// Its commands, for buttons, the input and the agent (X10).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<DeclaredCommand>,
    /// Set when an agent drafted it (X12): who, and in which chat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written_by: Option<WrittenBy>,
    /// Its vendor's MAVLink messages (S60): a dialect file beside the
    /// manifest, such as `acme.xml`, in MAVLink's XML. They are decoded and
    /// sent by name like the ArduPilot dialect's while it runs; an id or a
    /// name either already has is refused at install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialect: Option<String>,
    /// The hosts it may fetch from over HTTPS (S170), such as
    /// `api.adsb.lol`: names only, no scheme, port or path. Shown before it
    /// is installed; a fetch from any other is refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
    /// It is a traffic source (S171, ADR-0034): Airspace lists it, on or
    /// off, with its switch.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub traffic_source: bool,
    /// Installed off, until a person turns it on (S171): for a plugin
    /// UXDuck includes that sends anything from this computer.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub starts_off: bool,
    /// Its preflight checks (S194, ADR-0038 K12): reported per vehicle with
    /// `Effects.checks`, listed in each vehicle's checks under its name. A
    /// `block` check stops arming only if a person allowed it (K13);
    /// otherwise it warns.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<DeclaredCheck>,
}

/// A preflight check a plugin declares (S194, ADR-0038 K12).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeclaredCheck {
    /// Lower case, digits and dashes, unique in the plugin, such as `wind`.
    pub id: String,
    /// What passing means, for people: "Wind under 8 m/s".
    pub title: String,
    /// `block` asks to stop arming while it does not pass; it does only
    /// once a person allows the plugin's checks to (K13), and warns until
    /// then.
    pub severity: crate::dto::preflight::CheckSeverity,
    /// A report older than this many milliseconds no longer counts: the
    /// check is unknown until the plugin reports again. 1 s to a day; a
    /// minute if left out.
    #[serde(default = "default_stale_ms")]
    pub stale_ms: u32,
}

const fn default_stale_ms() -> u32 {
    60_000
}

/// Where a plugin says one of its checks stands for a vehicle (S194).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ReportedStatus {
    Pass,
    Fail,
    /// Something under way that ends by itself, such as a first reading.
    Waiting,
}

/// One of a plugin's declared checks for a vehicle, as it stands now
/// (S194). The latest report of each counts until its `staleMs` runs out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReportedCheck {
    pub vehicle_id: String,
    /// The id it declares in `plugin.json`'s `checks`; any other is
    /// ignored.
    pub id: String,
    pub status: ReportedStatus,
    /// What it found, in a few words without live numbers ("Over 8 m/s."):
    /// the vehicle's list is kept each time its words change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// An agent's draft: which agent wrote it, and in which chat.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WrittenBy {
    /// The agent's name, such as "UXDuck agent" or "Claude Code".
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

/// A manifest without `abi` was built for v1.
const fn abi_one() -> u32 {
    1
}

const fn default_cadence() -> u32 {
    200
}

/// What a plugin receives.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Subscriptions {
    /// Telemetry items by key or pattern, such as `battery.*`: the
    /// default level, already in units, in `on_items`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<ItemSubscription>,
    /// MAVLink messages by name, such as `GLOBAL_POSITION_INT`, decoded,
    /// in `on_messages`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages: Option<MessageSubscription>,
    /// Raw frames by message id, the payload in base64, in `on_frames`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames: Option<FrameSubscription>,
    /// `on_tick` every this many milliseconds, whatever arrives, such as
    /// 1000 for a clock (S101). At least 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tick_ms: Option<u32>,
}

/// What `on_start` receives once, after `describe` (S104): what it kept
/// with `Effects.store`, as it left it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StartInput {
    pub stored: Map<String, Value>,
    /// The daemon's clock, in milliseconds since 1970.
    #[schemars(schema_with = "crate::safe::integer")]
    pub now_ms: u64,
}

/// A page to fetch (S170): `GET` over HTTPS from a host the manifest
/// names. The answer comes to `on_fetched` with the same `id`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FetchRequest {
    /// The plugin's own name for it, given back with the answer.
    pub id: String,
    pub url: String,
}

/// What `on_fetched` receives (S170): the answer to a `fetch`, or why
/// there was none.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FetchedInput {
    pub id: String,
    pub url: String,
    /// The HTTP status; absent when no answer came.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Its headers, names in lower case.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub headers: std::collections::BTreeMap<String, String>,
    /// The body as text, at most 1 MB.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    /// Why there is no answer: refused, unreachable, too slow or too large.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The daemon's clock, in milliseconds since 1970.
    #[schemars(schema_with = "crate::safe::integer")]
    pub now_ms: u64,
}

/// An aircraft a plugin reports (S170, ADR-0034), as a traffic source of its
/// own. Only `address` is required; the picture merges it with the others.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginTraffic {
    /// Its ICAO 24-bit address, six hexadecimal digits.
    pub address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callsign: Option<String>,
    /// Its transponder code, four octal digits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub squawk: Option<String>,
    #[serde(default)]
    pub category: crate::dto::traffic::TrafficCategory,
    #[serde(default)]
    pub protocol: crate::dto::traffic::TrafficProtocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latitude_deg: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub longitude_deg: Option<f64>,
    /// Metres in the stated altitude reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub altitude_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub altitude_frame: Option<crate::dto::traffic::TrafficAltitudeFrame>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_deg: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ground_speed_mps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub climb_mps: Option<f64>,
    /// How long ago it was heard, in seconds; now when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen_ago_s: Option<f64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub simulated: bool,
}

/// What `on_tick` receives (S101).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TickInput {
    /// Present only for traffic sources, when the user selected locations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traffic: Option<crate::dto::traffic::TrafficFeedContext>,
    /// The daemon's clock, in milliseconds since 1970: `Date.now()`.
    #[schemars(schema_with = "crate::safe::integer")]
    pub now_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ItemSubscription {
    /// Keys, or a prefix ending in `*`.
    pub keys: Vec<String>,
    /// How often items are delivered, in milliseconds: the latest of each.
    #[serde(default = "default_cadence")]
    pub cadence_ms: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MessageSubscription {
    /// MAVLink's message names, from the ArduPilot dialect.
    pub names: Vec<String>,
    /// How often messages are delivered, in milliseconds: every one since
    /// the last batch.
    #[serde(default = "default_cadence")]
    pub cadence_ms: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FrameSubscription {
    /// MAVLink message ids, for messages the dialect does not have.
    pub ids: Vec<u32>,
    #[serde(default = "default_cadence")]
    pub cadence_ms: u32,
}

/// A command the plugin offers (X10). `on_command` receives its parameters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeclaredCommand {
    /// Lower case, digits and dashes, such as `photo`.
    pub name: String,
    /// For people: "Take a photo".
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// A JSON Schema for its parameters; none when it takes none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
    /// How much it can change (ADR-0020 A5), never less than the commands
    /// it sends.
    pub risk: Risk,
    /// A key it suggests, such as `Alt+C` (X14). Kept only for a `none`
    /// command, and left unbound when it clashes with one of UXDuck's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shortcut: Option<String>,
}

/// `tool/search`: plugin commands found by words (X11).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolSearchParams {
    /// Words from what the command does, such as "photo" or "zoom".
    pub query: String,
    /// At most this many, 10 if left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// One plugin command, with what `plugin_call` needs to run it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FoundTool {
    pub plugin_id: String,
    pub plugin_name: String,
    /// The command's name, for `plugin_call`'s `command`.
    pub command: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// A JSON Schema for `plugin_call`'s `params`; none when it takes none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
    pub risk: Risk,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolSearchResult {
    pub tools: Vec<FoundTool>,
}

/// How a widget or a map feature stands out: the host owns the look
/// (ADR-0006 X5), so a tone is the only choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Tone {
    Muted,
    Warning,
}

/// What a panel shows (X5). Every widget may have `help`, plain text shown
/// behind an info icon, and a tone. Items are the focused vehicle's, by key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum Widget {
    /// Fixed words: a heading, or a note.
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        heading: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tone: Option<Tone>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        help: Option<String>,
    },
    /// A telemetry item, a number in its unit or words.
    Value {
        item: String,
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tone: Option<Tone>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        help: Option<String>,
    },
    /// A number between `min` and `max`, warning outside `warnBelow` and
    /// `warnAbove`.
    Gauge {
        item: String,
        label: String,
        min: f64,
        max: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        warn_below: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        warn_above: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tone: Option<Tone>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        help: Option<String>,
    },
    /// An item over the last `windowS` seconds.
    Chart {
        item: String,
        label: String,
        #[serde(default = "default_window")]
        window_s: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tone: Option<Tone>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        help: Option<String>,
    },
    /// Rows under `columns`: the item's text is a JSON list of rows, each a
    /// list of cells, as the SDK's `table` writes it.
    Table {
        item: String,
        label: String,
        columns: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tone: Option<Tone>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        help: Option<String>,
    },
    /// One item, large, filling its tab: a clock, a stopwatch (S104). A
    /// `second` counts on between updates when the item has a rate.
    Display {
        item: String,
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tone: Option<Tone>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        help: Option<String>,
    },
    /// Calls the plugin's `on_command` with `command`, and `params` if set,
    /// such as `{ "minutes": 5 }` (S101).
    Button {
        label: String,
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        params: Option<Map<String, Value>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tone: Option<Tone>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        help: Option<String>,
    },
    /// A time of day to pick, with a Set button: calls `command` with
    /// `{ "at": … }`, the next moment the picker's clock reads that time,
    /// in seconds since 1970, so the plugin needs no time zone. An alarm.
    TimeInput {
        label: String,
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tone: Option<Tone>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        help: Option<String>,
    },
}

const fn default_window() -> u32 {
    60
}

/// A declarative panel: the host draws it from a fixed widget set.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Panel {
    pub id: String,
    pub title: String,
    pub widgets: Vec<Widget>,
    /// A tab of its own rather than a part of Control (S101). A plugin's
    /// tab panels share one tab, offered in "+" under the plugin's name,
    /// with a switch between them when there are several (S104). A tab
    /// follows no vehicle: its widgets show the plugin's own items
    /// (`ProducedItem` with no vehicle), and its buttons call the plugin
    /// with none.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tab: bool,
    /// A tab that follows a vehicle (S105): the selected one, or the one
    /// the tab is pinned to, as Telemetry does. Its widgets show that
    /// vehicle's items, then the plugin's own; its buttons call the plugin
    /// about it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub vehicle: bool,
}

/// What `describe` returns.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Description {
    #[serde(default)]
    pub panels: Vec<Panel>,
}

/// A telemetry item for `on_items`: the latest value of a key it subscribed to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ItemInput {
    pub vehicle_id: String,
    pub key: String,
    pub label: String,
    pub unit: Unit,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub time_us: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ItemBatch {
    pub items: Vec<ItemInput>,
}

/// A MAVLink message for `on_messages`, decoded: MAVLink's own field names
/// and raw units, such as `relative_alt` in millimetres (X3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MessageInput {
    pub vehicle_id: String,
    pub system_id: u8,
    pub component_id: u8,
    /// Such as `GLOBAL_POSITION_INT`.
    pub name: String,
    pub fields: Map<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MessageBatch {
    pub messages: Vec<MessageInput>,
    /// Messages were dropped since the last batch because the plugin was
    /// slow: its view has a gap.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gap: bool,
}

/// One MAVLink frame for `on_frames`. `payload` is base64, as received:
/// MAVLink 2 drops trailing zeros, so read missing bytes as zero.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Frame {
    pub vehicle_id: String,
    pub message_id: u32,
    pub system_id: u8,
    pub component_id: u8,
    pub payload: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FrameBatch {
    pub frames: Vec<Frame>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gap: bool,
}

/// What `on_command` receives: a panel button, `plugin/command`, or later
/// the input and the agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommandCall {
    pub command: String,
    /// None when it came from the plugin's own tab, which follows no
    /// vehicle (S101).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    /// Its parameters, as its schema describes; `{}` when none.
    #[serde(default = "empty_object")]
    pub params: Value,
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

/// A command for a vehicle, sent through the ordinary command path as the
/// plugin, with its capabilities and approvals.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SendCommand {
    pub vehicle_id: String,
    pub command: Command,
}

/// A MAVLink message by name, encoded by the host. Only messages the
/// manifest lists in `sends` go; fields left out are zero.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SendMessage {
    pub vehicle_id: String,
    pub name: String,
    #[serde(default)]
    pub fields: Map<String, Value>,
}

/// A telemetry item the plugin produces for a vehicle; it appears in the
/// vehicle panel like any other. Its key ends in its unit's suffix, as
/// `home.distance_m` (ADR-0006 X2). With no vehicle it is the plugin's
/// own, such as a stopwatch's time (S101): shown by its tabs, live, and
/// never journaled.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProducedItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    pub key: String,
    pub label: String,
    pub unit: Unit,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// How fast `value` changes per second until the next one, such as 1
    /// for a running stopwatch or -1 for a timer (S104): the app counts on
    /// in between, so the plugin need not send every tenth. Kept only for
    /// the plugin's own items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_per_s: Option<f64>,
}

/// A notification from the plugin, under its own name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginNotification {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    pub severity: Severity,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// Something a plugin draws on the map (X4): a `GeoJSON` geometry in
/// longitude and latitude, replaced whenever the plugin sends the same id
/// again. Drawn in the plugin's own layer, never as a panel over the map.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MapFeature {
    pub id: String,
    /// `Point`, `LineString`, `Polygon` or their `Multi` forms.
    pub geometry: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tone: Option<Tone>,
    /// Polygons filled faintly as well as outlined.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fill: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dashed: bool,
}

/// One plugin's own items (S101): those it produced with no vehicle, the
/// latest of each key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginItems {
    pub plugin_id: String,
    pub items: Vec<crate::dto::telemetry::TelemetryItem>,
}

/// `plugin/items`: every running plugin's own items; changes follow as
/// `plugin/itemsChanged`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginItemsResult {
    pub plugins: Vec<PluginItems>,
}

/// One plugin's features, as the map draws them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginLayer {
    pub plugin_id: String,
    /// The plugin's name, for the map's layer list.
    pub name: String,
    pub features: Vec<MapFeature>,
}

/// `plugin/features`: every plugin's layer; changes follow as
/// `plugin/featuresChanged`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginFeaturesResult {
    pub layers: Vec<PluginLayer>,
}

/// What every plugin function except `describe` returns.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Effects {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<SendCommand>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<SendMessage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<ProducedItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notifications: Vec<PluginNotification>,
    /// Features to draw or replace, by id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<MapFeature>,
    /// Ids of features to take off the map.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_features: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub log: Vec<String>,
    /// Keys to keep across restarts of the plugin and of UXDuck, a value
    /// each; `null` removes a key (S104). At most 64 KB in all, kept on
    /// this computer; `on_start` gets them back.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub store: Map<String, Value>,
    /// Call `on_tick` once in this many milliseconds, at least 100 (S104):
    /// a wake-up for a plugin that has nothing to do until then, such as a
    /// timer until it ends. The latest asked wins; a `tickMs` cadence goes
    /// on as well.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_in_ms: Option<u32>,
    /// Pages to fetch from hosts the manifest names (S170); each answer
    /// comes to `on_fetched`. At most four at a time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fetch: Vec<FetchRequest>,
    /// Aircraft it reports (S170, ADR-0034), joining the traffic picture
    /// while the plugin is on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub traffic: Vec<PluginTraffic>,
    /// How its traffic source is doing (S171), shown in its Airspace row
    /// until it says otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traffic_status: Option<PluginTrafficStatus>,
    /// Where its declared checks stand, per vehicle (S194, ADR-0038 K12):
    /// the latest of each counts until its `staleMs` runs out. A check it
    /// does not declare is ignored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<ReportedCheck>,
}

/// How a plugin's traffic source is doing (S171).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginTrafficStatus {
    /// One line, such as "312 of 500 requests today" or "Cannot reach
    /// adsb.lol".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The note is a problem: drawn as one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub problem: bool,
    /// Who to credit wherever its traffic is shown, such as
    /// `adsb.lol (ODbL)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credit: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PluginStatus {
    Starting,
    Running,
    /// Trapped, timed out or would not load; `detail` says why.
    Failed,
    /// Turned off by an operator (S104).
    Off,
}

/// An installed plugin.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Plugin {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Where it was installed from.
    pub path: String,
    pub capabilities: Vec<Capability>,
    pub subscribe: Subscriptions,
    pub sends: Vec<String>,
    pub commands: Vec<DeclaredCommand>,
    pub status: PluginStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub panels: Vec<Panel>,
    /// Its client record: what it does is journaled under
    /// `plugin:<id>`.
    pub client_id: String,
    /// Set when an agent drafted it (X12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written_by: Option<WrittenBy>,
    /// What it asks for beyond `capabilities`, waiting for an operator
    /// here: `plugin/allow` grants it (X13).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waiting: Vec<Capability>,
    /// Set when it came from another computer of the operator's or a link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<PluginOrigin>,
    /// Whether it runs (S104), left out when it does; `plugin/setEnabled`
    /// turns it off and on.
    #[serde(default = "on", skip_serializing_if = "Clone::clone")]
    pub enabled: bool,
    /// Shipped with UXDuck (S104): turned off, never removed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub included: bool,
    /// The hosts it may fetch from (S170).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
    /// It is a traffic source (S171).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub traffic_source: bool,
    /// Its preflight checks (S194).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<DeclaredCheck>,
    /// Whether its `block` checks may stop arming (K13): a person's answer
    /// on this computer, asked at install or on the Plugins page. Absent
    /// until someone answers; until `true` they only warn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks_block: Option<bool>,
}

const fn on() -> bool {
    true
}

/// `plugin/setEnabled` (S104).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginSetEnabledParams {
    pub id: String,
    pub enabled: bool,
}

/// `plugin/setChecksBlock` (S194, ADR-0038 K13): whether a plugin's
/// blocking checks may stop arming, or only warn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginSetChecksBlockParams {
    pub id: String,
    pub block: bool,
}

/// What a running plugin has used lately (S104), as the Plugins page
/// shows it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginUsage {
    pub plugin_id: String,
    /// Calls in the last minute.
    pub calls_per_min: u32,
    /// The share of the last minute it spent running, 0 to 100.
    pub busy_pct: f64,
    /// Its slowest call in the last minute, in milliseconds.
    pub slowest_ms: f64,
    /// What it keeps with `Effects.store`, in bytes.
    pub storage_bytes: u32,
    /// The most it may keep, in bytes.
    pub storage_limit_bytes: u32,
    /// The most memory it may use, in bytes.
    pub memory_limit_bytes: u32,
}

/// `plugin/usage`: each running plugin's.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginUsageResult {
    pub plugins: Vec<PluginUsage>,
}

/// Where a plugin came from (X13).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum PluginOrigin {
    /// Installed on another of the operator's computers.
    Synced { digest: String },
    /// A link someone shared.
    Shared { shared_by: String, token: String },
}

/// `plugin/share`: a link to an installed plugin.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginShareParams {
    pub id: String,
}

/// A link to a plugin (X13 "Shared"); the app's page for it is
/// `/plugins/shared/<token>`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginShare {
    pub token: String,
    pub plugin_id: String,
    pub name: String,
    pub version: String,
    pub digest: String,
    /// What its manifest asks for.
    pub capabilities: Vec<Capability>,
    pub created_at: String,
    /// The account that shared it, by name; set when someone else opens it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_by: Option<String>,
}

/// The account's live links.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginSharesResult {
    pub shares: Vec<PluginShare>,
}

/// `plugin/shared`, `plugin/installShared` and `plugin/unshare`: a link's token.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginTokenParams {
    pub token: String,
}

/// `plugin/draft` (X12): a TypeScript plugin written into UXDuck's own
/// folder of drafts, for `plugin/test` and then `plugin/install`. Drafting
/// the same id again makes a new version.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginDraftParams {
    /// Lower case, digits and dashes, such as `home-distance`.
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The whole TypeScript source: an ES module exporting any of
    /// `describe`, `onItems`, `onMessages`, `onFrames` and `onCommand`,
    /// importing helpers only from "@uxduck/sdk".
    pub script: String,
    /// What it may do without asking; `read` if left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<Capability>>,
    #[serde(default)]
    pub subscribe: Subscriptions,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sends: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<DeclaredCommand>,
    /// The chat it is written in; the app's agent fills this in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

/// A draft, as `plugin/drafts` lists it and `plugin/draft` answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginDraft {
    pub id: String,
    pub name: String,
    pub version: String,
    /// The folder to give `plugin/install`.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written_by: Option<WrittenBy>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub updated_at_us: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginDraftsResult {
    pub drafts: Vec<PluginDraft>,
}

/// `plugin/test`: runs a draft as the daemon would, against a vehicle's
/// last five minutes, and says what it did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginTestParams {
    /// The draft's id.
    pub id: String,
    /// Whose last five minutes; the first vehicle heard if left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    /// A command to call after the flight, with its parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// One item a tested plugin produced: its last value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TestedItem {
    pub key: String,
    pub label: String,
    pub unit: Unit,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// How many times it was produced.
    pub count: u32,
}

/// What a test run did, kept short for the agent.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginTestResult {
    /// The flight it ran against, in seconds; 0 when there was none.
    pub flight_s: f64,
    /// Calls made, by function.
    pub calls: std::collections::BTreeMap<String, u32>,
    pub panels: Vec<Panel>,
    pub items: Vec<TestedItem>,
    /// Titles of its notifications, in order, at most 20.
    pub notifications: Vec<String>,
    pub features: u32,
    /// The last report of each check it declares, by vehicle, at most 20,
    /// such as "wind on tlog-1: fail: Over 8 m/s." (S194).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<String>,
    /// Its last log lines, at most 20.
    pub log: Vec<String>,
    /// Why it stopped, if it did: a syntax error, a throw, too long.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginInstallParams {
    /// A directory holding `plugin.json` and the module it names.
    pub path: String,
    /// A person's answer: may its blocking checks stop arming (S194)?
    /// Only an operator's counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks_block: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginCommandParams {
    pub plugin_id: String,
    pub command: String,
    /// None from the plugin's own tab, which follows no vehicle (S101).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    /// Its parameters, checked against the command's schema by the plugin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

//! Operators on the wire (ADR-0005).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::dto::telemetry::AltitudeFrame;

/// The name of the operator the page makes for someone who has none, so
/// they never have to choose one (`DEFAULT_OPERATOR_NAME` in the web app).
/// A browser's work moving into an installed UXDuck that already has an
/// operator leaves this stand-in behind, or every move would add one.
pub const DEFAULT_OPERATOR_NAME: &str = "Operator";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum UnitSystem {
    Metric,
    Imperial,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CoordinateFormat {
    Decimal,
    Dms,
    Mgrs,
    Utm,
}

/// The map an operator flies over: imagery, or grey streets that follow the
/// theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Basemap {
    Imagery,
    Streets,
}

/// What a tab shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TabKind {
    /// The chat with its input. Closed by default and opened on request as
    /// the dock's left column; the one tab of its kind.
    Chat,
    /// The home: Get started, one screen at a time, with the globe behind
    /// it. Opened in the drawer by default, or as a tab on request.
    Home,
    /// The vehicles on a flat map.
    Map,
    /// The vehicles over terrain on a 3D globe: "3D map". Opened beside the
    /// map by the map's "3D" button, or from "+". Layouts stored with a Map
    /// in 3D come back with a 3D map tab in its place.
    #[serde(rename = "map-3d")]
    Map3d,
    /// A close view of the selected vehicle as a live model, without a map.
    VehicleView,
    /// The home view's pixel Earth as a tab of its own: every vehicle with a
    /// position on a whole, centred globe, the chosen one faced. Pressing a
    /// vehicle chooses it; pressing a region sends the map there.
    Globe,
    /// The focused vehicle's readings, as tiles (S65). Layouts stored with a
    /// `vehicle` tab come back with Telemetry and Control in its place.
    Telemetry,
    /// Everything that acts on the focused vehicle (S65).
    Control,
    /// The focused vehicle's own messages, every one, searchable (S26).
    Messages,
    /// The focused vehicle's files over MAVLink FTP (S121).
    Files,
    /// Every vehicle and link.
    Vehicles,
    /// The focused vehicle's parameters.
    Parameters,
    /// The focused vehicle's mission.
    Mission,
    /// Gamepads: claims, targets and mappings.
    Controllers,
    /// The focused vehicle's flight instruments as Mission Planner draws
    /// them (S43): "Instruments (classic)".
    Instruments,
    /// The focused vehicle's modern flight display (S77): "Instruments".
    /// Speed, altitude and heading as numbers, a quiet horizon, the flight
    /// path, and a chip for each value outside its threshold.
    FlightDisplay,
    /// The focused vehicle's history: notes and what UXDuck recorded (S69).
    History,
    /// The focused vehicle's camera (S75): a real vehicle's video (S38), or
    /// a simulated vehicle's 3D world (S35). Layouts stored with a `3d` or
    /// `video` tab come back with a Camera tab in its place.
    Camera,
    /// What to try first, for someone new: above the chat in the first-run
    /// layout, gone once closed.
    GetStarted,
    /// The focused vehicle's MAVLink traffic: the latest message of each
    /// type, both ways, with its rate and fields.
    Inspector,
    /// Settings as a tab (S87): the Settings page in the dock, from "+" or
    /// the page's "Open as a tab". Follows no vehicle.
    Settings,
    /// A plugin's panel as a tab of its own (S101), from "+": which one is
    /// the tab's `panel`. Follows a vehicle when its panel says so (S105).
    Plugin,
    /// One telemetry item, large (S105): which is the tab's `item`. Follows
    /// the selected vehicle, or the one it is pinned to.
    Readout,
    /// Telemetry items over time (S113): which, and over how long, is the
    /// tab's `chart`. Follows the selected vehicle, or the one it is pinned
    /// to.
    Chart,
    /// Practice exercises (S100, ADR-0024 X8). Follows the selected
    /// vehicle, or the one it is pinned to.
    Practice,
    /// The Places page as a tab, from the page's "Open as a tab". Follows
    /// no vehicle.
    Places,
    /// The Flights page as a tab. Follows no vehicle.
    Flights,
    /// The Missions page as a tab: every mission, not the focused
    /// vehicle's (`Mission`). Follows no vehicle.
    Missions,
    /// The ground under a plan or a mission, with heights (ADR-0029 P9).
    Profile,
    /// A rubber duck, turning: found only by typing "duck" in the command
    /// palette. Follows no vehicle.
    Duck,
}

/// One tab: what it shows and, when pinned, the vehicle it shows instead of
/// the selected one. Any number of tabs of a kind, the chat aside: there is
/// one chat, with the id `chat`. At most 32 tabs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Tab {
    /// Unique in the layout; up to 64 bytes.
    pub id: String,
    pub kind: TabKind,
    /// Pinned to this vehicle; none follows the selected vehicle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    /// Unticked in the Layouts menu: kept in its place and not drawn.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    /// The camera a Camera tab shows: a video source's id, or `3d` for a
    /// simulated vehicle's 3D world; none shows the vehicle's own. Up to 64
    /// bytes; dropped from other kinds of tab.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera: Option<String>,
    /// The panel a Plugin tab shows: `<plugin id>/<panel id>`, up to 64
    /// bytes; dropped from other kinds of tab (S101).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel: Option<String>,
    /// The item a Readout tab shows, such as `flight.air_s`, up to 64
    /// bytes; dropped from other kinds of tab (S105).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    /// An instruments tab draws over its vehicle's camera, the one its
    /// Camera tab would show (S93); dropped from other kinds of tab.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub camera_behind: bool,
    /// What a Chart tab draws (S113); dropped from other kinds of tab.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chart: Option<ChartSpec>,
}

/// A Chart tab's series and time span (S113).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChartSpec {
    /// Up to eight, each item once; past that they are dropped.
    #[serde(default)]
    pub series: Vec<ChartSeries>,
    /// Seconds shown: 30, 60 or 300. None, or any other, is a minute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_s: Option<u16>,
}

/// One line on a chart.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChartSeries {
    /// A telemetry item as a tile names it, such as `battery.voltage_v` or
    /// `mavlink.VFR_HUD.climb`; up to 64 bytes.
    pub item: String,
    /// One of the eight series colours, 0 to 7, fixed when it was added.
    #[serde(default)]
    pub colour: u8,
    /// Its line is not drawn.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
}

/// Which way a split lays out its children.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    /// Side by side.
    Row,
    /// One above the other.
    Column,
}

/// One part of the layout: a split into children, or a group of tabs with
/// one of them showing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum DockNode {
    Split {
        direction: Direction,
        children: Vec<DockChild>,
    },
    Group {
        tabs: Vec<Tab>,
        /// Index into `tabs`.
        active: usize,
    },
}

/// A child of a split and its share of the split, in thousandths.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DockChild {
    pub size: u16,
    pub node: DockNode,
}

/// A named layout: at most 20 custom layouts plus the two built-in views.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SavedLayout {
    pub id: String,
    pub name: String,
    pub dock: DockNode,
    /// Built-in view metadata, absent on a layout saved by the person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<LayoutPreset>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum LayoutPresetKind {
    Simple,
    Advanced,
}

/// The version explicitly adopted, with its baseline for previews and resets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LayoutPreset {
    pub kind: LayoutPresetKind,
    pub version: u32,
    pub base: DockNode,
}

/// Groups of tabs split in rows and columns, the chat one of the tabs.
/// Nothing floats over anything else. With the pane closed only the chat's
/// group shows. The daemon tidies what it is given: repeated tabs and empty
/// parts go, and the chat is put back if it is missing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Layout {
    pub pane_open: bool,
    pub dock: DockNode,
    pub saved: Vec<SavedLayout>,
    /// Whether the person has moved, closed, added or hidden a tab; the
    /// daemon works it out from each change, and then never arranges the
    /// dock itself (S65).
    #[serde(default)]
    pub arranged: bool,
    /// Whether the dock opened on the flight layout for a first vehicle,
    /// which happens once. Set by the daemon.
    #[serde(default)]
    pub flight_opened: bool,
    /// The saved layout the person is on (S66): set by choosing or saving
    /// one, cleared by a reset, and dropped by the daemon when that layout
    /// is deleted. The dock may have changed since; the app compares.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
}

/// The kinds of vehicle with their own default tiles (S65).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum VehicleClass {
    Plane,
    Copter,
    Rover,
    Boat,
    Sub,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RuleTest {
    Below,
    AtLeast,
    Equals,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RuleColour {
    Red,
    Amber,
    Green,
}

/// One colour rule; the first that matches fills the tile.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ColourRule {
    pub test: RuleTest,
    pub value: f64,
    pub colour: RuleColour,
}

impl Eq for ColourRule {}

/// One tile of the Telemetry tab: a telemetry item, in the operator's units.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Tile {
    /// The telemetry item's key, such as `battery.remaining_pct`.
    pub item: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decimals: Option<u8>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<ColourRule>,
}

/// A telemetry layout (S65): the tiles a Telemetry tab shows. At most 32,
/// each with at most 64 tiles of at most 8 rules.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryLayout {
    pub id: String,
    pub name: String,
    /// The vehicle type it is the operator's own default for; none for one
    /// made to choose for particular vehicles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<VehicleClass>,
    pub tiles: Vec<Tile>,
}

/// The layout chosen for one vehicle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleTelemetryChoice {
    pub vehicle_id: String,
    pub layout_id: String,
}

/// A group of parameters the operator tunes together, such as Hover tuning
/// (S74). A name of 1 to 40 characters, unique; at most 200 parameters,
/// each 1 to 16 characters of `A-Z`, `0-9` and `_`, in the order added.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParameterGroup {
    pub name: String,
    pub parameters: Vec<String>,
}

/// A local named profile: preferences and layout.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Operator {
    pub id: String,
    pub name: String,
    pub units: UnitSystem,
    pub coordinate_format: CoordinateFormat,
    /// The altitude the instruments show: above home unless chosen
    /// otherwise. Telemetry tiles name their own frame.
    #[serde(default)]
    pub altitude_frame: AltitudeFrame,
    pub layout: Layout,
    /// Whether place names typed in the input go to the geocoder.
    pub place_search: bool,
    pub basemap: Basemap,
    /// Shortcuts the operator changed. Actions not listed keep their default.
    pub shortcuts: Vec<ShortcutChange>,
    /// Actions whose confirmation the operator chose not to see again, by
    /// action id, such as `vehicle:take-off` (ADR-0020 A6).
    pub skipped_confirmations: Vec<String>,
    /// The operator's telemetry layouts (S65).
    #[serde(default)]
    pub telemetry_layouts: Vec<TelemetryLayout>,
    /// The layout chosen for each vehicle that has one.
    #[serde(default)]
    pub vehicle_telemetry: Vec<VehicleTelemetryChoice>,
    /// Groups of parameters the operator tunes together, such as Hover
    /// tuning (S74), by name.
    #[serde(default)]
    pub parameter_groups: Vec<ParameterGroup>,
    /// MAVLink messages the operator pinned to the top of the inspector, as
    /// `received:HEARTBEAT` or `sent:COMMAND_LONG`.
    #[serde(default)]
    pub inspector_pinned: Vec<String>,
    /// The Control tab's buttons per vehicle type, in the operator's order.
    /// A type not listed shows UXDuck's own.
    #[serde(default)]
    pub control_actions: Vec<ControlActions>,
    /// Bumped on every change. Send it back as `expectedVersion` when editing.
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OperatorListResult {
    pub operators: Vec<Operator>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OperatorCreateParams {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units: Option<UnitSystem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinate_format: Option<CoordinateFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub altitude_frame: Option<AltitudeFrame>,
}

/// Changes the fields given. Refused with `version-conflict` unless
/// `expectedVersion` is the current version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OperatorUpdateParams {
    pub id: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub expected_version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units: Option<UnitSystem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinate_format: Option<CoordinateFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub altitude_frame: Option<AltitudeFrame>,
    /// The whole layout; panels change together.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<Layout>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place_search: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basemap: Option<Basemap>,
    /// Every changed shortcut; replaces the ones saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shortcuts: Option<Vec<ShortcutChange>>,
    /// Every skipped confirmation; replaces the ones saved. Empty resets
    /// them all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped_confirmations: Option<Vec<String>>,
    /// Every telemetry layout; replaces the ones saved (S65).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry_layouts: Option<Vec<TelemetryLayout>>,
    /// Every vehicle's chosen layout; replaces the ones saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_telemetry: Option<Vec<VehicleTelemetryChoice>>,
    /// Every parameter group; replaces the ones saved (S74). At most 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_groups: Option<Vec<ParameterGroup>>,
    /// Every pinned inspector message; replaces the ones saved. At most 200,
    /// each `received:` or `sent:` and a message name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inspector_pinned: Option<Vec<String>>,
    /// Every type's Control tab buttons; replaces the ones saved. Each type
    /// once, at most 40 actions each, by action id such as
    /// `vehicle:mode:guided`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_actions: Option<Vec<ControlActions>>,
}

/// The Control tab's buttons for one type of vehicle, in order, by action
/// id: `vehicle:arm` (arm or disarm), `vehicle:take-off`, `vehicle:rtl`,
/// `vehicle:land`, `vehicle:reboot` or `vehicle:mode:<mode in lower case>`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ControlActions {
    pub class: VehicleClass,
    pub actions: Vec<String>,
}

/// One action's shortcut as the operator set it (ADR-0020 A4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ShortcutChange {
    /// The action's id, such as `open-map`.
    pub action: String,
    /// A key such as `Alt+G`, or null where the operator cleared it.
    pub shortcut: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OperatorRemoveParams {
    pub id: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub expected_version: u64,
}

/// Shows a tab of a kind in an operator's layout, as "+" does: the one
/// there, else a new one (S43). The operator is the chat's, else the one
/// given; with neither, the chat's window opens it in its own layout.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LayoutOpenParams {
    pub kind: TabKind,
    /// The chat asking; the agent's turns fill it in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_id: Option<String>,
}

/// The operator whose layout now shows the tab; none when the chat's
/// window opens it in its own.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LayoutOpenResult {
    #[serde(default)]
    pub operator_id: Option<String>,
    pub kind: TabKind,
}

/// Which operator this connection acts as; journal entries name it.
/// `null` acts as nobody in particular.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClientSetOperatorParams {
    pub operator_id: Option<String>,
}

/// Locally attributed activity; not inferred flight time or cloud ownership.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OperatorProfileParams {
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OperatorProfile {
    pub operator: Operator,
    #[schemars(schema_with = "crate::safe::integer")]
    pub commands_requested: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub notes_written: u64,
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub last_active_us: Option<u64>,
}

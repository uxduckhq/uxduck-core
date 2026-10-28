//! Operators: local named profiles on this machine (ADR-0005). Every journal
//! entry names the operator as well as the client, so attribution is right
//! from the first entity. Layouts join in S6, and become a tree in S63.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The single units choice operators had before ADR-0036. Kept as a
/// summary of `quantity_units` for older apps, and read for an operator
/// saved before them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnitSystem {
    #[default]
    Metric,
    Imperial,
}

impl UnitSystem {
    /// Every quantity in this system (ADR-0036 U3): what an operator
    /// saved with only this choice shows, and what the helper sets.
    #[must_use]
    pub const fn quantity_units(self) -> QuantityUnits {
        match self {
            Self::Metric => QuantityUnits {
                distance: DistanceUnit::Kilometers,
                altitude: AltitudeUnit::Meters,
                speed: SpeedUnit::MetersPerSecond,
                vertical_speed: VerticalSpeedUnit::MetersPerSecond,
                temperature: TemperatureUnit::Celsius,
            },
            Self::Imperial => QuantityUnits {
                distance: DistanceUnit::Miles,
                altitude: AltitudeUnit::Feet,
                speed: SpeedUnit::Knots,
                vertical_speed: VerticalSpeedUnit::FeetPerMinute,
                temperature: TemperatureUnit::Fahrenheit,
            },
        }
    }
}

/// Horizontal distances (ADR-0036): short ones in the first unit, long
/// ones in the second.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DistanceUnit {
    /// Metres, then kilometres.
    #[default]
    Kilometers,
    /// Feet, then statute miles.
    Miles,
}

/// Altitudes and heights.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AltitudeUnit {
    #[default]
    Meters,
    Feet,
}

/// Horizontal speeds: ground speed, airspeed, wind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SpeedUnit {
    #[default]
    MetersPerSecond,
    KilometersPerHour,
    Knots,
    MilesPerHour,
}

/// Climb and descent rates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VerticalSpeedUnit {
    #[default]
    MetersPerSecond,
    FeetPerMinute,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TemperatureUnit {
    #[default]
    Celsius,
    Fahrenheit,
}

/// The unit an operator reads each kind of quantity in (ADR-0036): the
/// stored choice, one per quantity. Presentation only; contracts stay SI.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuantityUnits {
    #[serde(default)]
    pub distance: DistanceUnit,
    #[serde(default)]
    pub altitude: AltitudeUnit,
    #[serde(default)]
    pub speed: SpeedUnit,
    #[serde(default)]
    pub vertical_speed: VerticalSpeedUnit,
    #[serde(default)]
    pub temperature: TemperatureUnit,
}

impl QuantityUnits {
    /// The old single choice these read as, for apps that know only it
    /// (ADR-0036 U4): by the altitude unit, the one a pilot flies by.
    #[must_use]
    pub const fn system(self) -> UnitSystem {
        match self.altitude {
            AltitudeUnit::Meters => UnitSystem::Metric,
            AltitudeUnit::Feet => UnitSystem::Imperial,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CoordinateFormat {
    #[default]
    Decimal,
    Dms,
    Mgrs,
    Utm,
}

/// The map an operator flies over (ADR-0014 M2). Imagery is the minimum for
/// flying, so it is the default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Basemap {
    #[default]
    Imagery,
    /// Grey streets, light or dark with the theme.
    Streets,
}

/// What a tab shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TabKind {
    /// The chat with its input (S63). Closed by default and opened on
    /// request as the dock's left column (maintainer, 2026-10-05); the one
    /// tab of its kind, under its own name.
    Chat,
    /// The home (maintainer, 2026-10-05): Get started, one screen at a
    /// time, with the globe behind it. Opened in the drawer by default,
    /// or as a tab on request.
    Home,
    /// The vehicles on a flat map, always on `MapLibre` (maintainer, 2026-10-04).
    Map,
    /// The vehicles over terrain on a 3D globe, always on Cesium (maintainer,
    /// 2026-10-04): "3D map", opened beside the map by its "3D" button. A Map
    /// stored in 3D before then comes back as one of these in its place.
    #[serde(rename = "map-3d")]
    Map3d,
    /// A close view of the selected vehicle as a live model, without a map.
    VehicleView,
    /// The home view's pixel Earth as a tab of its own (maintainer,
    /// 2026-10-04): every vehicle with a position on a whole, centred globe,
    /// the chosen one faced. The quick picker from the chat, without the chat
    /// around it: pressing a vehicle chooses it, pressing a region sends the
    /// map there.
    Globe,
    /// The focused vehicle's readings, as tiles (S65).
    Telemetry,
    /// Everything that acts on the focused vehicle (S65).
    Control,
    /// The focused vehicle's own messages (S26).
    Messages,
    /// The focused vehicle's files over MAVLink FTP (S121).
    Files,
    /// Only in layouts stored before S65: tidying replaces it with
    /// Telemetry and Control in its place.
    #[serde(rename = "vehicle")]
    LegacyVehicle,
    /// Every vehicle and link.
    Vehicles,
    /// The focused vehicle's parameters.
    Parameters,
    /// The focused vehicle's mission.
    Mission,
    /// Gamepads: claims, targets and mappings.
    Controllers,
    /// Only in layouts stored before S75 (the 3D view, S35): tidying makes
    /// it a 3D map tab in its place, where the vehicle flies as its model
    /// (2026-10-05; a Camera tab from S75 until then).
    #[serde(rename = "3d")]
    LegacyView3d,
    /// The focused vehicle's flight instruments as Mission Planner draws
    /// them (S43): "Instruments (classic)".
    Instruments,
    /// The focused vehicle's modern flight display (S77): "Instruments".
    FlightDisplay,
    /// The focused vehicle's history: notes and what UXDuck recorded (S69).
    History,
    /// Only in layouts stored before S75 (the Video tab, S38): tidying makes
    /// it a Camera tab in its place.
    #[serde(rename = "video")]
    LegacyVideo,
    /// The focused vehicle's camera (S75): its video.
    Camera,
    /// What to try first, from layouts stored while it was a tab above the
    /// chat. It moved into the chat (maintainer, 2026-10-03), so a stored
    /// layout comes back without it.
    GetStarted,
    /// The focused vehicle's MAVLink traffic: the latest message of each
    /// type, both ways, with its rate and fields.
    Inspector,
    /// Settings as a tab beside the others (S87), for someone who wants it
    /// beside the map. Settings opens as a page of its own; this is the
    /// same page in the dock. It follows no vehicle.
    Settings,
    /// A plugin's panel as a tab of its own (S101), opened from "+": which
    /// one is the tab's `panel`. It follows a vehicle when the panel says so
    /// (S105).
    Plugin,
    /// One telemetry item, large (S105): which is the tab's `item`. It
    /// follows the selected vehicle, or the one it is pinned to.
    Readout,
    /// Telemetry items over time (S113): which, and over how long, is the
    /// tab's `chart`. It follows the selected vehicle, or the one it is
    /// pinned to.
    Chart,
    /// Practice exercises on a simulated vehicle (S100, ADR-0024 X8). It
    /// follows the selected vehicle, or the one it is pinned to.
    Practice,
    /// The Places page as a tab, for keeping it beside the map. Places opens
    /// as a page of its own; this is the page's "Open as a tab".
    Places,
    /// The Flights page as a tab.
    Flights,
    /// The Missions page as a tab: every mission, where `Mission` is the
    /// focused vehicle's.
    Missions,
    /// The ground under a plan, or the selected vehicle's mission, with its
    /// heights and clearance (ADR-0029 P9). Opened below the map.
    Profile,
    /// A rubber duck, turning as the new simulation form's vehicles do:
    /// found only by typing "duck" in the command palette (maintainer,
    /// 2026-10-12). It follows no vehicle.
    Duck,
    /// Other aircraft near the vehicles and the sources that report them
    /// (S168, ADR-0034). It follows no vehicle.
    Airspace,
    /// The focused vehicle's setup (S184, ADR-0037 V1): a row per
    /// procedure, such as calibrating its compasses. It follows the selected
    /// vehicle, or the one it is pinned to.
    Setup,
    /// The focused vehicle's checklist (S193, ADR-0038), with its other
    /// preflight checks, as a tab of its own when it is long. It follows
    /// the selected vehicle, or the one it is pinned to.
    Checklist,
}

impl TabKind {
    /// The kind's name on the wire, which is also the id of a layout's first
    /// tab of it.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Home => "home",
            Self::Map => "map",
            Self::Map3d => "map-3d",
            Self::VehicleView => "vehicle-view",
            Self::Globe => "globe",
            Self::Telemetry => "telemetry",
            Self::Control => "control",
            Self::Messages => "messages",
            Self::Files => "files",
            Self::LegacyVehicle => "vehicle",
            Self::Vehicles => "vehicles",
            Self::Parameters => "parameters",
            Self::Mission => "mission",
            Self::Controllers => "controllers",
            Self::LegacyView3d => "3d",
            Self::Instruments => "instruments",
            Self::FlightDisplay => "flight-display",
            Self::History => "history",
            Self::LegacyVideo => "video",
            Self::Camera => "camera",
            Self::GetStarted => "get-started",
            Self::Inspector => "inspector",
            Self::Settings => "settings",
            Self::Plugin => "plugin",
            Self::Readout => "readout",
            Self::Chart => "chart",
            Self::Practice => "practice",
            Self::Places => "places",
            Self::Flights => "flights",
            Self::Missions => "missions",
            Self::Profile => "profile",
            Self::Duck => "duck",
            Self::Airspace => "airspace",
            Self::Setup => "setup",
            Self::Checklist => "checklist",
        }
    }

    /// The kind whose group a new tab of this kind opens in, when there is
    /// one: the inspector beside Messages, the camera below the maps with
    /// Parameters (maintainer, 2026-10-16), and Setup beside Parameters
    /// (S184), and the checklist beside the Cockpit, where Arm is (S193).
    /// The web's `JOINS`.
    #[must_use]
    pub const fn joins(self) -> Option<Self> {
        match self {
            Self::Inspector => Some(Self::Messages),
            Self::Camera | Self::Setup => Some(Self::Parameters),
            Self::Checklist => Some(Self::Control),
            _ => None,
        }
    }
}

/// One tab: what it shows and, when pinned, the vehicle it shows instead of
/// the selected one. Any number of tabs of a kind, the chat aside.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "StoredTab")]
pub struct Tab {
    /// Unique in the layout.
    pub id: String,
    pub kind: TabKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    /// Unticked in the Layouts menu (S65): kept in its place, not drawn,
    /// and shown there again when ticked.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    /// The camera a Camera tab was set to show (S75): a video source's id,
    /// or `3d` for a simulated vehicle's 3D world. None shows the vehicle's
    /// own. Kept with the tab, so each Camera tab comes back as it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera: Option<String>,
    /// The panel a Plugin tab shows (S101): `<plugin id>/<panel id>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel: Option<String>,
    /// The item a Readout tab shows (S105), such as `flight.air_s`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    /// An instruments tab draws over its vehicle's camera (S93).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub camera_behind: bool,
    /// What a Chart tab draws (S113).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chart: Option<ChartSpec>,
}

/// A Chart tab's series and time span (S113).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChartSpec {
    #[serde(default)]
    pub series: Vec<ChartSeries>,
    /// How many seconds it shows: 30, 60 or 300. None is a minute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_s: Option<u16>,
}

/// One line on a chart: a telemetry item, as a tile names it, and its
/// colour, fixed when it was added so it never shifts when another goes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChartSeries {
    pub item: String,
    /// One of the eight series colours, 0 to 7.
    #[serde(default)]
    pub colour: u8,
    /// Its line is not drawn.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
}

/// The most series a chart keeps, one per colour.
pub const MAX_CHART_SERIES: usize = 8;
/// The spans a chart may show, in seconds: what UXDuck keeps is five minutes.
const CHART_SPANS: [u16; 3] = [30, 60, 300];

impl ChartSpec {
    /// Drops series with an empty or overlong item, repeats and those past
    /// eight; gives a colour out of range the first one free; forgets a span
    /// not offered.
    fn tidy(&mut self) {
        let mut seen: Vec<String> = vec![];
        self.series.retain(|s| {
            let keep = !s.item.is_empty()
                && s.item.len() <= MAX_ID_LEN
                && !seen.contains(&s.item)
                && seen.len() < MAX_CHART_SERIES;
            if keep {
                seen.push(s.item.clone());
            }
            keep
        });
        let mut used = [false; MAX_CHART_SERIES];
        for series in &mut self.series {
            let colour = usize::from(series.colour);
            if colour < MAX_CHART_SERIES && !used[colour] {
                used[colour] = true;
                continue;
            }
            let free = used.iter().position(|u| !u).unwrap_or(0);
            used[free] = true;
            series.colour = u8::try_from(free).unwrap_or(0);
        }
        if self.span_s.is_some_and(|s| !CHART_SPANS.contains(&s)) {
            self.span_s = None;
        }
    }
}

/// A tab as stored: itself, or just its kind, as layouts were until tabs
/// could repeat. `threeD` marked a Map drawn in 3D until 3D became a tab
/// of its own (maintainer, 2026-10-04).
#[derive(Deserialize)]
#[serde(untagged)]
enum StoredTab {
    Kind(TabKind),
    #[serde(rename_all = "camelCase")]
    Tab {
        id: String,
        kind: TabKind,
        #[serde(default)]
        vehicle_id: Option<String>,
        #[serde(default)]
        hidden: bool,
        #[serde(default)]
        three_d: bool,
        #[serde(default)]
        camera: Option<String>,
        #[serde(default)]
        panel: Option<String>,
        #[serde(default)]
        item: Option<String>,
        #[serde(default)]
        camera_behind: bool,
        #[serde(default)]
        chart: Option<ChartSpec>,
    },
}

impl From<StoredTab> for Tab {
    fn from(stored: StoredTab) -> Self {
        match stored {
            StoredTab::Kind(kind) => Self::of(kind),
            StoredTab::Tab {
                id,
                kind,
                vehicle_id,
                hidden,
                three_d,
                camera,
                panel,
                item,
                camera_behind,
                chart,
            } => Self {
                id,
                // A Map drawn in 3D before 2026-10-04 is a 3D map tab, in its place.
                kind: if kind == TabKind::Map && three_d {
                    TabKind::Map3d
                } else {
                    kind
                },
                vehicle_id,
                hidden,
                camera,
                panel,
                item,
                camera_behind,
                chart,
            },
        }
    }
}

impl Tab {
    /// A tab of this kind that follows the selected vehicle, named after its kind.
    #[must_use]
    pub fn of(kind: TabKind) -> Self {
        Self {
            id: kind.slug().to_owned(),
            kind,
            vehicle_id: None,
            hidden: false,
            camera: None,
            panel: None,
            item: None,
            camera_behind: false,
            chart: None,
        }
    }

    /// A Vehicle tab from before S65 becomes Telemetry and Control, pinned
    /// alike: named after their kinds when it was the first, else after it.
    fn split_vehicle(self) -> [Self; 2] {
        let id = |kind: TabKind| {
            if self.id == TabKind::LegacyVehicle.slug() {
                kind.slug().to_owned()
            } else {
                format!("{}-{}", self.id, kind.slug())
            }
        };
        [TabKind::Telemetry, TabKind::Control].map(|kind| Self {
            id: id(kind),
            kind,
            vehicle_id: self.vehicle_id.clone(),
            hidden: self.hidden,
            camera: None,
            panel: None,
            item: None,
            camera_behind: false,
            chart: None,
        })
    }

    /// Drops a pin, camera or panel that is empty or too long, a camera on
    /// a tab other than a Camera tab, and a panel on one other than a
    /// Plugin tab.
    fn drop_bad_ids(&mut self) {
        let bad = |id: &Option<String>| {
            id.as_ref()
                .is_some_and(|v| v.is_empty() || v.len() > MAX_ID_LEN)
        };
        if bad(&self.vehicle_id) {
            self.vehicle_id = None;
        }
        if self.kind != TabKind::Camera || bad(&self.camera) {
            self.camera = None;
        }
        if self.kind != TabKind::Plugin || bad(&self.panel) {
            self.panel = None;
        }
        if self.kind != TabKind::Readout || bad(&self.item) {
            self.item = None;
        }
        if !matches!(self.kind, TabKind::Instruments | TabKind::FlightDisplay) {
            self.camera_behind = false;
        }
        if self.kind == TabKind::Chart {
            if let Some(chart) = &mut self.chart {
                chart.tidy();
            }
        } else {
            self.chart = None;
        }
    }
}

/// The most tabs a layout keeps.
pub const MAX_TABS: usize = 32;
/// The longest tab, vehicle or camera id kept, in bytes.
const MAX_ID_LEN: usize = 64;

/// Which way a split lays out its children.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    /// Side by side.
    Row,
    /// One above the other.
    Column,
}

/// One part of the layout: a split into children, or a group of tabs with
/// one of them showing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockChild {
    pub size: u16,
    pub node: DockNode,
}

/// A layout the operator saved under a name, to come back to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedLayout {
    pub id: String,
    pub name: String,
    pub dock: DockNode,
    /// A built-in view, with the adopted version and its original arrangement.
    /// The working dock may be customised without changing this baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<LayoutPreset>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LayoutPresetKind {
    Simple,
    Advanced,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutPreset {
    pub kind: LayoutPresetKind,
    pub version: u32,
    pub base: DockNode,
}

/// Increment when a shipped view's default arrangement changes.
/// 2 (2026-10-12): Advanced ends its Messages group with the MAVLink
/// inspector and a Chart.
/// 3 (2026-10-15): Simple stacks the map above Telemetry, Control and History.
/// 4 (2026-10-17): Simple includes Controllers beside Vehicle Control.
/// 5 (2026-10-17): Simple puts Control and Telemetry side by side.
/// 6 (2026-10-18): Simple shows the 3D map beside the map.
/// 7 (2026-10-18): Simple keeps four panes and gains Advanced's Messages,
/// Flight display, Instruments and Parameters as tabs.
/// 8 (2026-10-18): Built-in layouts migrate automatically; named copies stay.
/// 9 (2026-10-18): Simple and Advanced keep a Camera tab, not showing, last in
/// Parameters' group.
/// 10 (2026-10-18): the page's phone layouts keep one too; the desktop's are
/// unchanged, the number matching the page's.
/// 11 (2026-10-22): Simple and both phone presets include Mission as a tab.
/// 12 (2026-10-23): combine Mission tabs with Airspace in built-in presets.
pub const LAYOUT_PRESET_VERSION: u32 = 12;

/// The longest saved layout name, in characters.
pub const MAX_LAYOUT_NAME_CHARS: usize = 40;
/// The most saved layouts an operator keeps.
pub const MAX_SAVED_LAYOUTS: usize = 20;
/// What every split's children's sizes add up to.
pub const SIZE_TOTAL: u16 = 1000;

/// The operator's layout (ADR-0009, as changed for S63): groups of tabs split
/// in rows and columns, the chat one of the tabs. Nothing floats over
/// anything else. With the pane closed only the chat's group shows, and the
/// rest waits as it was.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "StoredLayout")]
pub struct Layout {
    pub pane_open: bool,
    pub dock: DockNode,
    pub saved: Vec<SavedLayout>,
    /// Whether the person has moved, closed, added or hidden a tab (S65):
    /// UXDuck then never arranges the dock for them.
    #[serde(default)]
    pub arranged: bool,
    /// Whether the dock opened on the flight layout for a first vehicle,
    /// which it does once.
    #[serde(default)]
    pub flight_opened: bool,
    /// The saved layout the person is on (S66): chosen or saved last, kept
    /// while the dock changes, and gone with that layout or a reset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
}

/// A layout as stored: the tree, or the two groups beside the chat that
/// layouts were before S63.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredLayout {
    #[serde(default)]
    pane_open: bool,
    dock: Option<DockNode>,
    #[serde(default)]
    saved: Vec<SavedLayout>,
    pane_share: Option<u8>,
    split_share: Option<u8>,
    #[serde(default)]
    groups: Vec<OldGroup>,
    #[serde(default)]
    arranged: bool,
    #[serde(default)]
    flight_opened: bool,
    #[serde(default)]
    current: Option<String>,
}

#[derive(Deserialize)]
struct OldGroup {
    tabs: Vec<Tab>,
    active: usize,
}

impl From<StoredLayout> for Layout {
    fn from(stored: StoredLayout) -> Self {
        let mut dock = stored.dock.unwrap_or_else(|| {
            if stored.groups.is_empty() {
                return Layout::initial().dock;
            }
            // The chat, then the pane's groups, side by side in their shares.
            let pane = u16::from(stored.pane_share.unwrap_or(72).clamp(25, 85)) * 10;
            let second = u16::from(stored.split_share.unwrap_or(40).clamp(20, 80));
            let two = stored.groups.len() > 1;
            let mut children = vec![DockChild {
                size: SIZE_TOTAL - pane,
                node: DockNode::of(&[TabKind::Chat]),
            }];
            for (i, group) in stored.groups.into_iter().take(2).enumerate() {
                let share = match (two, i) {
                    (false, _) => 100,
                    (true, 0) => 100 - second,
                    (true, _) => second,
                };
                children.push(DockChild {
                    size: pane * share / 100,
                    node: DockNode::Group {
                        tabs: group.tabs,
                        active: group.active,
                    },
                });
            }
            DockNode::Split {
                direction: Direction::Row,
                children,
            }
        });
        // The flight layout is there from the first run (maintainer,
        // 2026-10-10). Only an untouched old first run moves to it: the Home
        // tab alone, or the map waiting for a first vehicle.
        let mut flight_opened = stored.flight_opened;
        let waiting = dock == DockNode::of(&[TabKind::Home]) || dock == Layout::first_run_dock();
        if !stored.arranged && !stored.flight_opened && waiting {
            dock = Layout::flight_dock();
            flight_opened = true;
        }
        Self {
            pane_open: stored.pane_open,
            dock,
            saved: stored.saved,
            arranged: stored.arranged,
            flight_opened,
            current: stored.current,
        }
        .normalised()
    }
}

impl DockNode {
    /// A group showing its first tab.
    #[must_use]
    pub fn group(tabs: Vec<Tab>) -> Self {
        Self::Group { tabs, active: 0 }
    }

    /// A group of one tab of each of these kinds, following the selection.
    #[must_use]
    pub fn of(kinds: &[TabKind]) -> Self {
        Self::group(kinds.iter().copied().map(Tab::of).collect())
    }

    /// The arrangement without sizes or the showing tab: which tabs, hidden
    /// or not, in which groups and splits. Two layouts with the same shape
    /// differ only by a drag of a divider or a click on a tab.
    #[must_use]
    pub fn shape(&self) -> String {
        match self {
            Self::Group { tabs, .. } => {
                let tabs: Vec<String> = tabs
                    .iter()
                    .map(|t| {
                        format!(
                            "{}:{}:{}",
                            t.id,
                            t.kind.slug(),
                            if t.hidden { "h" } else { "" }
                        )
                    })
                    .collect();
                format!("[{}]", tabs.join(","))
            }
            Self::Split {
                direction,
                children,
            } => {
                let inner: Vec<String> = children.iter().map(|c| c.node.shape()).collect();
                format!("{direction:?}({})", inner.join(","))
            }
        }
    }

    fn tabs(&self) -> Vec<&Tab> {
        match self {
            Self::Group { tabs, .. } => tabs.iter().collect(),
            Self::Split { children, .. } => children.iter().flat_map(|c| c.node.tabs()).collect(),
        }
    }

    fn has_tab(&self, id: &str) -> bool {
        self.tabs().iter().any(|t| t.id == id)
    }

    fn has_more_than_home(&self) -> bool {
        self.tabs()
            .iter()
            .any(|t| !matches!(t.kind, TabKind::Chat | TabKind::Home))
    }

    /// Makes the first tab of this kind (an unpinned one first) the one its
    /// group shows, and shows it if hidden. Whether there was one.
    fn show(&mut self, kind: TabKind) -> bool {
        let tabs = self.tabs();
        let of_kind: Vec<&&Tab> = tabs.iter().filter(|t| t.kind == kind).collect();
        let Some(target) = of_kind
            .iter()
            .find(|t| t.vehicle_id.is_none() && !t.hidden)
            .or_else(|| of_kind.iter().find(|t| !t.hidden))
            .or_else(|| of_kind.first())
            .map(|t| t.id.clone())
        else {
            return false;
        };
        self.show_id(&target);
        true
    }

    fn show_id(&mut self, id: &str) {
        match self {
            Self::Group { tabs, active } => {
                if let Some(index) = tabs.iter().position(|t| t.id == id) {
                    tabs[index].hidden = false;
                    *active = index;
                }
            }
            Self::Split { children, .. } => {
                for child in children {
                    child.node.show_id(id);
                }
            }
        }
    }

    /// Adds a tab, showing, to the first group without the chat. Whether
    /// there was one.
    fn add_to_first_group(&mut self, tab: &Tab) -> bool {
        match self {
            Self::Group { tabs, active } => {
                // Nothing joins the chat.
                if tabs.iter().any(|t| t.kind == TabKind::Chat) {
                    return false;
                }
                tabs.push(tab.clone());
                *active = tabs.len() - 1;
                true
            }
            Self::Split { children, .. } => children
                .iter_mut()
                .any(|child| child.node.add_to_first_group(tab)),
        }
    }

    /// Adds a tab to the first group holding a tab of `kind` and not the
    /// chat, showing it. Whether there was one.
    fn add_to_group_with(&mut self, kind: TabKind, tab: &Tab) -> bool {
        match self {
            Self::Group { tabs, active } => {
                // Nothing joins the chat.
                if !tabs.iter().any(|t| t.kind == kind)
                    || tabs.iter().any(|t| t.kind == TabKind::Chat)
                {
                    return false;
                }
                tabs.push(tab.clone());
                *active = tabs.len() - 1;
                true
            }
            Self::Split { children, .. } => children
                .iter_mut()
                .any(|child| child.node.add_to_group_with(kind, tab)),
        }
    }

    /// Whether a tab of this kind is anywhere in this part.
    #[must_use]
    pub fn contains(&self, kind: TabKind) -> bool {
        match self {
            Self::Group { tabs, .. } => tabs.iter().any(|t| t.kind == kind),
            Self::Split { children, .. } => children.iter().any(|c| c.node.contains(kind)),
        }
    }

    /// Drops repeated tabs (by id, and any chat after the first), empty
    /// parts and tabs past `MAX_TABS`, folds a split of one child into it
    /// and a split into a parent going the same way, and scales sizes so a
    /// split's add up to `SIZE_TOTAL`. `None` if nothing is left.
    fn tidy(self, seen: &mut Vec<String>) -> Option<Self> {
        match self {
            Self::Group { tabs, active } => {
                let mut shown = tabs.get(active).map(|t| t.id.clone());
                // A Vehicle tab is Telemetry with Control beside it, Telemetry showing.
                let tabs: Vec<Tab> = tabs
                    .into_iter()
                    .flat_map(|tab| {
                        if tab.kind != TabKind::LegacyVehicle {
                            return vec![tab];
                        }
                        let split = tab.clone().split_vehicle();
                        if shown.as_deref() == Some(tab.id.as_str()) {
                            shown = Some(split[0].id.clone());
                        }
                        split.to_vec()
                    })
                    // A Video tab from before S75 is a Camera tab, in its place; a
                    // 3D tab from before it a 3D map tab, since the vehicle's model
                    // flies there now (2026-10-05).
                    .map(|mut tab| {
                        if tab.kind == TabKind::LegacyVideo {
                            tab.kind = TabKind::Camera;
                        } else if tab.kind == TabKind::LegacyView3d {
                            tab.kind = TabKind::Map3d;
                        }
                        tab
                    })
                    .collect();
                let mut kept: Vec<Tab> = Vec::with_capacity(tabs.len());
                for mut tab in tabs {
                    // Get started is in the chat now; its old tab goes.
                    if tab.kind == TabKind::GetStarted {
                        continue;
                    }
                    let chat_again = tab.kind == TabKind::Chat
                        && seen.iter().any(|id| id == TabKind::Chat.slug());
                    if tab.id.is_empty()
                        || tab.id.len() > MAX_ID_LEN
                        || seen.contains(&tab.id)
                        || chat_again
                        || seen.len() >= MAX_TABS
                    {
                        continue;
                    }
                    // The chat is one tab, always under its own name.
                    if tab.kind == TabKind::Chat {
                        TabKind::Chat.slug().clone_into(&mut tab.id);
                        tab.vehicle_id = None;
                    }
                    tab.drop_bad_ids();
                    seen.push(tab.id.clone());
                    kept.push(tab);
                }
                if kept.is_empty() {
                    return None;
                }
                let active = shown
                    .and_then(|id| kept.iter().position(|t| t.id == id))
                    .unwrap_or(0);
                Some(Self::Group { tabs: kept, active })
            }
            Self::Split {
                direction,
                children,
            } => {
                let mut kept: Vec<(u32, DockNode)> = Vec::with_capacity(children.len());
                for child in children {
                    let size = u32::from(child.size.max(1));
                    match child.node.tidy(seen) {
                        None => {}
                        // The same way as this split: its children join this one's.
                        Some(Self::Split {
                            direction: inner,
                            children: grandchildren,
                        }) if inner == direction => {
                            let total: u32 = grandchildren.iter().map(|g| u32::from(g.size)).sum();
                            for g in grandchildren {
                                kept.push((
                                    (size * u32::from(g.size)).div_ceil(total.max(1)),
                                    g.node,
                                ));
                            }
                        }
                        Some(node) => kept.push((size, node)),
                    }
                }
                match kept.len() {
                    0 => None,
                    1 => kept.pop().map(|(_, node)| node),
                    _ => Some(Self::Split {
                        direction,
                        children: scaled(kept),
                    }),
                }
            }
        }
    }
}

/// Sizes in proportion, adding up to `SIZE_TOTAL`, none below one.
fn scaled(children: Vec<(u32, DockNode)>) -> Vec<DockChild> {
    let total: u32 = children.iter().map(|(size, _)| *size).sum::<u32>().max(1);
    let count = u32::try_from(children.len()).unwrap_or(u32::MAX);
    let total_size = u32::from(SIZE_TOTAL);
    let mut left = total_size;
    let last = children.len().saturating_sub(1);
    children
        .into_iter()
        .enumerate()
        .map(|(i, (size, node))| {
            let rest = count - u32::try_from(i).unwrap_or(0) - 1;
            let size = if i == last {
                left
            } else {
                (size * total_size / total).clamp(1, left - rest)
            };
            left -= size;
            DockChild {
                size: u16::try_from(size).unwrap_or(SIZE_TOTAL),
                node,
            }
        })
        .collect()
}

/// A tidy tree. The chat may be left out: the person closed it, and a new
/// chat brings it back (maintainer, 2026-10-02).
fn normalised_dock(dock: DockNode) -> DockNode {
    let mut seen = Vec::new();
    dock.tidy(&mut seen)
        .unwrap_or_else(|| Layout::initial().dock)
}

impl Layout {
    /// New operators start in Simple, with Home beside the workspace.
    /// The adopted preset and its baseline are persisted like saved layouts.
    /// An existing operator's arrangement is never replaced on load.
    #[must_use]
    pub fn initial() -> Self {
        let dock = Self::simple_dock();
        Self {
            pane_open: true,
            dock: dock.clone(),
            saved: vec![SavedLayout {
                id: "preset-simple".into(),
                name: "Simple".into(),
                dock: dock.clone(),
                preset: Some(LayoutPreset {
                    kind: LayoutPresetKind::Simple,
                    version: LAYOUT_PRESET_VERSION,
                    base: dock,
                }),
            }],
            arranged: false,
            flight_opened: true,
            current: Some("preset-simple".into()),
        }
    }

    /// The map and the 3D map (always, maintainer 2026-10-18) above
    /// Control (with Mission, Controllers, History and Parameters) beside Telemetry
    /// (with Messages, Flight display Instruments and Airspace): four panes, with
    /// Advanced's everyday tools as tabs (maintainer, 2026-10-18).
    #[must_use]
    pub fn simple_dock() -> DockNode {
        DockNode::Split {
            direction: Direction::Column,
            children: vec![
                DockChild {
                    size: 560,
                    node: DockNode::Split {
                        direction: Direction::Row,
                        children: vec![
                            DockChild {
                                size: 500,
                                node: DockNode::of(&[TabKind::Map]),
                            },
                            DockChild {
                                size: 500,
                                node: DockNode::of(&[TabKind::Map3d]),
                            },
                        ],
                    },
                },
                DockChild {
                    size: 440,
                    node: DockNode::Split {
                        direction: Direction::Row,
                        children: vec![
                            DockChild {
                                size: 500,
                                node: DockNode::of(&[
                                    TabKind::Control,
                                    TabKind::Mission,
                                    TabKind::Controllers,
                                    TabKind::History,
                                    TabKind::Parameters,
                                    TabKind::Camera,
                                ]),
                            },
                            DockChild {
                                size: 500,
                                node: DockNode::of(&[
                                    TabKind::Telemetry,
                                    TabKind::Messages,
                                    TabKind::FlightDisplay,
                                    TabKind::Instruments,
                                    TabKind::Airspace,
                                ]),
                            },
                        ],
                    },
                },
            ],
        }
    }

    /// The map alone: what the first run was before 2026-10-10, and what
    /// the UI draws when every tab is hidden.
    #[must_use]
    pub fn first_run_dock() -> DockNode {
        DockNode::of(&[TabKind::Map])
    }

    /// A flight tool's layout (maintainer, 2026-10-03): on top, the map and
    /// the camera; below, Instruments (with the classic instruments and the
    /// mission) and Messages (with parameters, controllers, then the MAVLink
    /// inspector and a Chart at the end; maintainer, 2026-10-12); on the
    /// right, Control short above Telemetry (with
    /// history and Airspace, maintainer 2026-10-21). No chat: it opens on request as the left column
    /// (maintainer, 2026-10-05). "Reset layout" brings it back.
    #[must_use]
    pub fn flight_dock() -> DockNode {
        let split = |direction, children: Vec<(u16, DockNode)>| DockNode::Split {
            direction,
            children: children
                .into_iter()
                .map(|(size, node)| DockChild { size, node })
                .collect(),
        };
        split(
            Direction::Row,
            vec![
                (
                    720,
                    split(
                        Direction::Column,
                        vec![
                            (
                                620,
                                split(
                                    Direction::Row,
                                    vec![
                                        (500, DockNode::of(&[TabKind::Map])),
                                        (500, DockNode::of(&[TabKind::Map3d])),
                                    ],
                                ),
                            ),
                            (
                                380,
                                split(
                                    Direction::Row,
                                    vec![
                                        (
                                            390,
                                            DockNode::of(&[
                                                TabKind::FlightDisplay,
                                                TabKind::Instruments,
                                                TabKind::Mission,
                                            ]),
                                        ),
                                        (
                                            610,
                                            DockNode::of(&[
                                                TabKind::Messages,
                                                TabKind::Parameters,
                                                TabKind::Controllers,
                                                TabKind::Inspector,
                                                TabKind::Chart,
                                                TabKind::Camera,
                                            ]),
                                        ),
                                    ],
                                ),
                            ),
                        ],
                    ),
                ),
                (
                    280,
                    split(
                        Direction::Column,
                        vec![
                            (400, DockNode::of(&[TabKind::Control])),
                            (
                                600,
                                DockNode::of(&[
                                    TabKind::Telemetry,
                                    TabKind::History,
                                    TabKind::Airspace,
                                ]),
                            ),
                        ],
                    ),
                ),
            ],
        )
    }

    /// The flight layout, with AI on its right when `from` has one.
    fn flight_dock_keeping_chat(from: &DockNode) -> DockNode {
        if !from.contains(TabKind::Chat) {
            return Self::flight_dock();
        }
        normalised_dock(DockNode::Split {
            direction: Direction::Row,
            children: vec![
                DockChild {
                    size: 700,
                    node: Self::flight_dock(),
                },
                DockChild {
                    size: 300,
                    node: DockNode::of(&[TabKind::Chat]),
                },
            ],
        })
    }

    /// Shows a tab of this kind, as "+" does: the first one there is, an
    /// unpinned one first, shown again if hidden; else a new one in the
    /// group of the kind it joins (the inspector beside Messages, the camera with Parameters), the
    /// first group without the chat, or beside everything. Opens the pane.
    pub fn open(&mut self, kind: TabKind) {
        if kind == TabKind::Chat {
            return;
        }
        // From the home alone, or the chat: the flight layout.
        if !self.dock.has_more_than_home() {
            self.dock = Self::flight_dock_keeping_chat(&self.dock);
        }
        self.pane_open = true;
        if self.dock.show(kind) {
            return;
        }
        let mut id = kind.slug().to_owned();
        let mut n = 2;
        while self.dock.has_tab(&id) {
            id = format!("{}-{n}", kind.slug());
            n += 1;
        }
        let tab = Tab {
            id,
            ..Tab::of(kind)
        };
        let joined = kind
            .joins()
            .is_some_and(|with| self.dock.add_to_group_with(with, &tab));
        if !joined && !self.dock.add_to_first_group(&tab) {
            let dock = std::mem::replace(&mut self.dock, DockNode::of(&[TabKind::Chat]));
            self.dock = normalised_dock(DockNode::Split {
                direction: Direction::Row,
                children: vec![
                    DockChild {
                        size: 667,
                        node: dock,
                    },
                    DockChild {
                        size: 333,
                        node: DockNode::group(vec![tab]),
                    },
                ],
            });
        }
    }

    /// Tidies the tree and bounds the saved layouts, so a stored layout
    /// always renders.
    #[must_use]
    pub fn normalised(mut self) -> Self {
        // Saved names are user-owned; built-ins follow each shipped revision.
        for saved in &mut self.saved {
            if let Some(preset) = &mut saved.preset
                && preset.version > 0
                && preset.version < LAYOUT_PRESET_VERSION
            {
                let dock = match preset.kind {
                    LayoutPresetKind::Simple => Self::simple_dock(),
                    LayoutPresetKind::Advanced => Self::flight_dock(),
                };
                saved.dock = dock.clone();
                preset.base = dock.clone();
                preset.version = LAYOUT_PRESET_VERSION;
                if self.current.as_ref() == Some(&saved.id) {
                    self.dock = dock;
                    self.pane_open = true;
                }
            }
        }
        self.dock = normalised_dock(self.dock);
        let mut ids: Vec<String> = Vec::new();
        let mut presets: Vec<LayoutPresetKind> = Vec::new();
        let mut custom_count = 0;
        self.saved = std::mem::take(&mut self.saved)
            .into_iter()
            .filter_map(|saved| {
                let name = clean_layout_name(&saved.name)?;
                if saved.id.is_empty() || ids.contains(&saved.id) {
                    return None;
                }
                let preset = if let Some(mut preset) = saved.preset {
                    if preset.version == 0 || presets.contains(&preset.kind) {
                        return None;
                    }
                    presets.push(preset.kind);
                    preset.base = normalised_dock(preset.base);
                    Some(preset)
                } else {
                    if custom_count >= MAX_SAVED_LAYOUTS {
                        return None;
                    }
                    custom_count += 1;
                    None
                };
                ids.push(saved.id.clone());
                Some(SavedLayout {
                    id: saved.id,
                    name,
                    dock: normalised_dock(saved.dock),
                    preset,
                })
            })
            .collect();
        // The layout the person is on goes with it.
        if self
            .current
            .as_ref()
            .is_some_and(|id| !self.saved.iter().any(|s| &s.id == id))
        {
            self.current = None;
        }
        self
    }
}

impl Default for Layout {
    fn default() -> Self {
        Self::initial()
    }
}

/// Trims a saved layout's name; `None` if it is empty or too long.
#[must_use]
pub fn clean_layout_name(name: &str) -> Option<String> {
    let name = name.trim();
    (!name.is_empty() && name.chars().count() <= MAX_LAYOUT_NAME_CHARS).then(|| name.to_owned())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Operator {
    pub id: String,
    pub name: String,
    /// The old single units choice: a summary of `quantity_units`, read
    /// only for an operator saved before them (ADR-0036 U4).
    pub units: UnitSystem,
    /// The unit for each kind of quantity (ADR-0036). None on an operator
    /// saved before them, which reads as `units` says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quantity_units: Option<QuantityUnits>,
    pub coordinate_format: CoordinateFormat,
    /// The altitude the instruments show (maintainer, 2026-10-05): above
    /// home unless chosen otherwise. Telemetry tiles name their own frame.
    /// Operators saved before it have none and get above home.
    #[serde(default)]
    pub altitude_frame: crate::domain::telemetry::AltitudeFrame,
    /// Operators saved before S6 have none and get the default.
    #[serde(default)]
    pub layout: Layout,
    /// Whether place names typed in the input go to the geocoder (ADR-0009:
    /// they can reveal operations). Coordinates and saved places work
    /// either way.
    #[serde(default = "place_search_default")]
    pub place_search: bool,
    /// Operators saved before the Esri maps have none and get imagery.
    #[serde(default)]
    pub basemap: Basemap,
    /// Shortcuts the operator changed, by action id (ADR-0020 A4): a key
    /// such as `Alt+G`, or none where they cleared it. Actions not here keep
    /// their default.
    #[serde(default)]
    pub shortcuts: BTreeMap<String, Option<String>>,
    /// Actions whose confirmation the operator chose not to see again
    /// (ADR-0020 A6), by action id. Only `confirm` actions are kept here.
    #[serde(default)]
    pub skipped_confirmations: Vec<String>,
    /// The operator's telemetry layouts (S65): their own per vehicle type,
    /// and any they made to choose for a vehicle.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub telemetry_layouts: Vec<crate::domain::telemetry_layout::TelemetryLayout>,
    /// The layout chosen for a vehicle, by vehicle id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vehicle_telemetry: BTreeMap<String, String>,
    /// Groups of parameters the operator tunes together, such as Hover
    /// tuning (S74), by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameter_groups: Vec<ParameterGroup>,
    /// MAVLink messages the operator pinned to the top of the inspector, as
    /// `received:HEARTBEAT` or `sent:COMMAND_LONG`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inspector_pinned: Vec<String>,
    /// The actions the Control tab shows as buttons, in the operator's
    /// order, per vehicle type: modes they pinned and the commands they
    /// kept. A type not here shows UXDuck's own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub control_actions: Vec<ControlActions>,
    /// The marker that is them (ADR-0035 M12), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub me_marker_id: Option<String>,
    /// Their display settings as the app keeps them (C1): a JSON document
    /// with their display preferences and saved layouts, written by the
    /// app and synced with the operator so it reaches their other
    /// computers. Opaque here; none until the app first writes it, when
    /// the fields above it that hold display choices are its old copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    /// The verified account this operator last signed in as, recorded by
    /// UXDuck from the server's answer and never set by a client; synced
    /// with the operator. Choosing another operator of the account signed
    /// in keeps the sign-in (maintainer, 2026-10-23).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// The mapping they use with each controller, kept on them rather than
    /// the controller (maintainer, 2026-10-23), so another person at the
    /// same computer keeps theirs. Recorded when they claim one with it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub controller_mappings: Vec<ControllerMapping>,
    /// Bumped on every change; writes carry the version they started from.
    pub version: u64,
}

const fn place_search_default() -> bool {
    true
}

impl Operator {
    /// The unit for each quantity: the stored ones, else as the old
    /// single choice says.
    #[must_use]
    pub const fn quantity_units(&self) -> QuantityUnits {
        match self.quantity_units {
            Some(units) => units,
            None => self.units.quantity_units(),
        }
    }

    /// Sets every quantity's unit, keeping the old choice as their summary.
    pub const fn set_quantity_units(&mut self, units: QuantityUnits) {
        self.quantity_units = Some(units);
        self.units = units.system();
    }
}

/// The longest operator name, in characters.
pub const MAX_NAME_CHARS: usize = 64;

/// Trims a proposed name; `None` if it is empty or too long.
#[must_use]
pub fn clean_name(name: &str) -> Option<String> {
    let name = name.trim();
    (!name.is_empty() && name.chars().count() <= MAX_NAME_CHARS).then(|| name.to_owned())
}

/// A group of parameters the operator tunes together, such as Hover tuning
/// (S74): its name and its parameters in the order they were added.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParameterGroup {
    pub name: String,
    pub parameters: Vec<String>,
}

/// The most parameter groups an operator keeps.
pub const MAX_PARAMETER_GROUPS: usize = 100;
/// The longest parameter group name, in characters.
pub const MAX_GROUP_NAME_CHARS: usize = 40;
/// The most parameters in one group.
pub const MAX_GROUP_PARAMETERS: usize = 200;
/// The longest parameter name, in characters, as in ArduPilot.
pub const MAX_PARAMETER_NAME_CHARS: usize = 16;

/// Parameter groups with their names trimmed, sorted by name, and each
/// parameter once; `None` if any is out of bounds or two share a name.
#[must_use]
pub fn clean_parameter_groups(groups: &[ParameterGroup]) -> Option<Vec<ParameterGroup>> {
    if groups.len() > MAX_PARAMETER_GROUPS {
        return None;
    }
    let parameter = |name: &String| {
        (1..=MAX_PARAMETER_NAME_CHARS).contains(&name.len())
            && name
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    };
    let mut kept: Vec<ParameterGroup> = Vec::with_capacity(groups.len());
    for group in groups {
        let name = group.name.trim();
        if name.is_empty()
            || name.chars().count() > MAX_GROUP_NAME_CHARS
            || group.parameters.len() > MAX_GROUP_PARAMETERS
            || !group.parameters.iter().all(parameter)
            || kept.iter().any(|k| k.name == name)
        {
            return None;
        }
        let mut parameters: Vec<String> = Vec::with_capacity(group.parameters.len());
        for p in &group.parameters {
            if !parameters.contains(p) {
                parameters.push(p.clone());
            }
        }
        kept.push(ParameterGroup {
            name: name.to_owned(),
            parameters,
        });
    }
    kept.sort_by(|a, b| a.name.cmp(&b.name));
    Some(kept)
}

/// The most messages pinned in the inspector.
pub const MAX_INSPECTOR_PINNED: usize = 200;
/// The longest message name in a pin, in characters.
pub const MAX_PINNED_MESSAGE_CHARS: usize = 64;

/// Pinned inspector messages, each once in the order given; `None` if any
/// is not `received:` or `sent:` and a message name, or there are too many.
#[must_use]
pub fn clean_inspector_pinned(pinned: &[String]) -> Option<Vec<String>> {
    if pinned.len() > MAX_INSPECTOR_PINNED {
        return None;
    }
    let valid = |key: &String| {
        let message = key
            .strip_prefix("received:")
            .or_else(|| key.strip_prefix("sent:"));
        message.is_some_and(|m| {
            (1..=MAX_PINNED_MESSAGE_CHARS).contains(&m.len())
                && m.bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_' || b == b'#')
        })
    };
    let mut kept: Vec<String> = Vec::with_capacity(pinned.len());
    for key in pinned {
        if !valid(key) {
            return None;
        }
        if !kept.contains(key) {
            kept.push(key.clone());
        }
    }
    Some(kept)
}

/// The Control tab's buttons for one type of vehicle, in order, by action
/// id such as `vehicle:arm` or `vehicle:mode:guided`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlActions {
    pub class: crate::domain::telemetry_layout::VehicleClass,
    pub actions: Vec<String>,
}

/// The mapping an operator uses with one controller: by the controller's
/// id on the computer where it was chosen, and its name, which finds the
/// same kind of controller on another computer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControllerMapping {
    pub controller_id: String,
    pub controller_name: String,
    pub mapping_id: String,
}

/// The most controllers an operator keeps a mapping for; the oldest goes.
pub const MAX_CONTROLLER_MAPPINGS: usize = 32;

impl Operator {
    /// The mapping this operator chose for a controller: for that one, else
    /// for one of the same name.
    #[must_use]
    pub fn mapping_for(&self, controller_id: &str, controller_name: &str) -> Option<&str> {
        self.controller_mappings
            .iter()
            .find(|m| m.controller_id == controller_id)
            .or_else(|| {
                self.controller_mappings
                    .iter()
                    .find(|m| m.controller_name == controller_name)
            })
            .map(|m| m.mapping_id.as_str())
    }

    /// Notes the mapping chosen for a controller, newest last; `false` if
    /// it was already the one.
    pub fn choose_mapping(
        &mut self,
        controller_id: &str,
        controller_name: &str,
        mapping_id: &str,
    ) -> bool {
        if self
            .controller_mappings
            .iter()
            .any(|m| m.controller_id == controller_id && m.mapping_id == mapping_id)
        {
            return false;
        }
        self.controller_mappings
            .retain(|m| m.controller_id != controller_id);
        self.controller_mappings.push(ControllerMapping {
            controller_id: controller_id.to_owned(),
            controller_name: controller_name.to_owned(),
            mapping_id: mapping_id.to_owned(),
        });
        let over = self
            .controller_mappings
            .len()
            .saturating_sub(MAX_CONTROLLER_MAPPINGS);
        self.controller_mappings.drain(..over);
        true
    }
}

/// The largest display document an operator carries, in bytes: well
/// under the account's limit for one synced change.
pub const MAX_DISPLAY_BYTES: usize = 256 * 1024;

/// The most buttons on the Control tab for one type of vehicle.
pub const MAX_CONTROL_ACTIONS: usize = 40;
/// The longest action id on the Control tab, in characters.
pub const MAX_CONTROL_ACTION_CHARS: usize = 64;

/// Each type once, sorted, with each action once in the order given;
/// `None` if a type comes twice, an id is not `vehicle:` and lower-case
/// letters, digits, `-`, `_`, `:` or spaces (PX4's `Precision Land`), or
/// there are too many.
#[must_use]
pub fn clean_control_actions(all: &[ControlActions]) -> Option<Vec<ControlActions>> {
    let valid = |id: &String| {
        id.len() <= MAX_CONTROL_ACTION_CHARS
            && id.strip_prefix("vehicle:").is_some_and(|rest| {
                !rest.is_empty()
                    && rest.bytes().all(|b| {
                        b.is_ascii_lowercase()
                            || b.is_ascii_digit()
                            || matches!(b, b'-' | b'_' | b':' | b' ')
                    })
            })
    };
    let mut kept: Vec<ControlActions> = Vec::with_capacity(all.len());
    for entry in all {
        if entry.actions.len() > MAX_CONTROL_ACTIONS
            || !entry.actions.iter().all(valid)
            || kept.iter().any(|k| k.class == entry.class)
        {
            return None;
        }
        let mut actions: Vec<String> = Vec::with_capacity(entry.actions.len());
        for id in &entry.actions {
            if !actions.contains(id) {
                actions.push(id.clone());
            }
        }
        kept.push(ControlActions {
            class: entry.class,
            actions,
        });
    }
    kept.sort_by_key(|k| k.class);
    Some(kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(tabs: &[TabKind]) -> DockNode {
        DockNode::of(tabs)
    }

    fn child(size: u16, node: DockNode) -> DockChild {
        DockChild { size, node }
    }

    fn row(children: Vec<DockChild>) -> DockNode {
        DockNode::Split {
            direction: Direction::Row,
            children,
        }
    }

    #[test]
    fn vehicle_view_is_kept_in_a_pinned_saved_layout() {
        let mut layout = Layout {
            dock: DockNode::group(vec![Tab {
                vehicle_id: Some("v".into()),
                ..Tab::of(TabKind::VehicleView)
            }]),
            ..Layout::default()
        };
        layout.saved.push(SavedLayout {
            id: "inspection".into(),
            name: "Inspection".into(),
            preset: None,
            dock: layout.dock.clone(),
        });
        let json = serde_json::to_value(&layout).unwrap();
        let restored: Layout = serde_json::from_value::<Layout>(json).unwrap().normalised();
        let tabs = restored.dock.tabs();
        let view = tabs
            .iter()
            .find(|tab| tab.kind == TabKind::VehicleView)
            .unwrap();
        assert_eq!(view.vehicle_id.as_deref(), Some("v"));
        assert_eq!(
            restored
                .saved
                .iter()
                .find(|s| s.id == "inspection")
                .unwrap()
                .dock,
            restored.dock
        );
        assert_eq!(TabKind::VehicleView.slug(), "vehicle-view");
    }

    #[test]
    fn a_layout_from_before_s63_becomes_the_chat_beside_its_groups() {
        let old = serde_json::json!({
            "paneOpen": true,
            "paneShare": 70,
            "splitShare": 40,
            "groups": [
                { "tabs": ["map", "mission"], "active": 1 },
                { "tabs": ["vehicle"], "active": 0 }
            ]
        });
        let layout: Layout = serde_json::from_value(old).unwrap();
        assert!(layout.pane_open);
        assert_eq!(
            layout.dock,
            row(vec![
                child(300, group(&[TabKind::Chat])),
                child(
                    420,
                    DockNode::Group {
                        tabs: vec![Tab::of(TabKind::Map), Tab::of(TabKind::Mission)],
                        active: 1
                    }
                ),
                child(280, group(&[TabKind::Telemetry, TabKind::Control])),
            ])
        );
        assert!(layout.saved.is_empty());
    }

    #[test]
    fn only_an_untouched_old_first_run_moves_to_the_flight_layout() {
        for waiting in [DockNode::of(&[TabKind::Home]), Layout::first_run_dock()] {
            let mut old = serde_json::to_value(Layout::initial()).unwrap();
            old["dock"] = serde_json::to_value(&waiting).unwrap();
            old["flightOpened"] = false.into();
            let migrated: Layout = serde_json::from_value(old.clone()).unwrap();
            assert_eq!(migrated.dock, Layout::flight_dock());
            assert!(migrated.flight_opened);
            old["arranged"] = true.into();
            let arranged: Layout = serde_json::from_value(old.clone()).unwrap();
            assert_eq!(arranged.dock, waiting);
            old["arranged"] = false.into();
            old["flightOpened"] = true.into();
            let opened: Layout = serde_json::from_value(old).unwrap();
            assert_eq!(opened.dock, waiting);
        }
    }

    #[test]
    fn a_layout_round_trips_through_json() {
        // The first run: the flight layout, Home in the UI drawer.
        let layout = Layout::initial();
        let json = serde_json::to_value(&layout).unwrap();
        assert_eq!(json["paneOpen"], true);
        assert_eq!(json["flightOpened"], true);
        assert_eq!(json["dock"]["type"], "split");
        assert_eq!(serde_json::from_value::<Layout>(json).unwrap(), layout);

        // The flight layout has no chat: the map first on the top row.
        let flight = serde_json::to_value(Layout::flight_dock()).unwrap();
        assert_eq!(flight["type"], "split");
        let top = &flight["children"][0]["node"]["children"][0]["node"]["children"];
        assert_eq!(top[0]["node"]["tabs"][0]["kind"], "map");
        assert!(!Layout::flight_dock().contains(TabKind::Chat));
    }

    #[test]
    fn a_tree_is_tidied_so_it_always_renders() {
        let messy = Layout {
            pane_open: true,
            dock: row(vec![
                child(1, group(&[TabKind::Chat, TabKind::Map])),
                // A repeated tab goes, and the group it leaves empty with it.
                child(1, group(&[TabKind::Map])),
                // A row in a row joins it; a column of one is that one.
                child(
                    2,
                    row(vec![
                        child(1, group(&[TabKind::LegacyVehicle])),
                        child(
                            1,
                            DockNode::Split {
                                direction: Direction::Column,
                                children: vec![child(5, group(&[TabKind::Mission]))],
                            },
                        ),
                    ]),
                ),
                // A 3D tab from before S75 is a 3D map tab, keeping its id.
                child(
                    0,
                    DockNode::Group {
                        tabs: vec![Tab::of(TabKind::LegacyView3d)],
                        active: 9,
                    },
                ),
            ]),
            saved: Vec::new(),
            arranged: false,
            flight_opened: false,
            current: None,
        }
        .normalised();
        assert_eq!(
            messy.dock,
            row(vec![
                child(250, group(&[TabKind::Chat, TabKind::Map])),
                child(250, group(&[TabKind::Telemetry, TabKind::Control])),
                child(250, group(&[TabKind::Mission])),
                child(
                    250,
                    DockNode::Group {
                        tabs: vec![Tab {
                            id: "3d".into(),
                            kind: TabKind::Map3d,
                            vehicle_id: None,
                            hidden: false,
                            camera: None,
                            panel: None,
                            item: None,
                            camera_behind: false,
                            chart: None,
                        }],
                        active: 0,
                    }
                ),
            ])
        );
    }

    #[test]
    fn a_layout_may_leave_the_chat_out() {
        let layout = Layout {
            pane_open: true,
            dock: group(&[TabKind::Map]),
            saved: vec![SavedLayout {
                id: "a".into(),
                name: "  Survey ".into(),
                preset: None,
                dock: row(vec![]),
            }],
            arranged: false,
            flight_opened: false,
            current: None,
        }
        .normalised();
        // The person closed it; a new chat brings it back.
        assert_eq!(layout.dock, group(&[TabKind::Map]));
        // A saved layout is kept, named cleanly, and empty becomes the first-run one.
        assert_eq!(layout.saved[0].name, "Survey");
        assert_eq!(layout.saved[0].dock, Layout::initial().dock);
    }

    #[test]
    fn a_layout_stored_with_get_started_comes_back_without_it() {
        // Get started moved into the chat; the web has no tab for it.
        let layout = Layout {
            pane_open: true,
            dock: DockNode::Split {
                direction: Direction::Column,
                children: vec![
                    child(550, group(&[TabKind::GetStarted])),
                    child(450, group(&[TabKind::Chat, TabKind::GetStarted])),
                ],
            },
            saved: Vec::new(),
            arranged: false,
            flight_opened: false,
            current: None,
        }
        .normalised();
        assert_eq!(layout.dock, group(&[TabKind::Chat]));
    }

    #[test]
    fn each_camera_tab_keeps_its_camera_and_no_other_tab_has_one() {
        let stored = serde_json::json!({
            "paneOpen": true,
            "dock": {
                "type": "group",
                "tabs": [
                    {"id": "camera", "kind": "camera", "camera": "udp-5600"},
                    {"id": "t2", "kind": "camera", "camera": "3d"},
                    {"id": "t3", "kind": "camera", "camera": ""},
                    {"id": "map", "kind": "map", "camera": "udp-5600"}
                ],
                "active": 0
            },
            "saved": []
        });
        let layout: Layout = serde_json::from_value(stored).unwrap();
        let DockNode::Group { tabs, .. } = &layout.dock else {
            panic!("a group")
        };
        let cameras: Vec<Option<&str>> = tabs.iter().map(|t| t.camera.as_deref()).collect();
        assert_eq!(cameras, vec![Some("udp-5600"), Some("3d"), None, None]);
        let again: Layout = serde_json::from_value(serde_json::to_value(&layout).unwrap()).unwrap();
        assert_eq!(again, layout);
    }

    #[test]
    fn a_readout_keeps_its_item_and_its_pin_and_no_other_tab_has_one() {
        let stored = serde_json::json!({
            "paneOpen": true,
            "dock": {
                "type": "group",
                "tabs": [
                    {"id": "readout", "kind": "readout", "item": "flight.air_s", "vehicleId": "v2"},
                    {"id": "t2", "kind": "readout"},
                    {"id": "map", "kind": "map", "item": "flight.air_s"}
                ],
                "active": 0
            },
            "saved": []
        });
        let layout: Layout = serde_json::from_value(stored).unwrap();
        let DockNode::Group { tabs, .. } = &layout.dock else {
            panic!("a group")
        };
        let items: Vec<Option<&str>> = tabs.iter().map(|t| t.item.as_deref()).collect();
        assert_eq!(items, vec![Some("flight.air_s"), None, None]);
        assert_eq!(tabs[0].vehicle_id.as_deref(), Some("v2"));
        let again: Layout = serde_json::from_value(serde_json::to_value(&layout).unwrap()).unwrap();
        assert_eq!(again, layout);
    }

    #[test]
    fn a_chart_keeps_its_series_and_span_and_no_other_tab_has_one() {
        let long = "x".repeat(65);
        let stored = serde_json::json!({
            "paneOpen": true,
            "dock": {
                "type": "group",
                "tabs": [
                    {"id": "chart", "kind": "chart", "vehicleId": "v2", "chart": {
                        "spanS": 300,
                        "series": [
                            {"item": "battery.voltage_v", "colour": 3},
                            {"item": "battery.voltage_v", "colour": 4},
                            {"item": "", "colour": 1},
                            {"item": long, "colour": 1},
                            {"item": "mavlink.VFR_HUD.climb", "colour": 3, "hidden": true},
                            {"item": "battery.current_a", "colour": 12}
                        ]
                    }},
                    {"id": "t2", "kind": "chart", "chart": {"spanS": 45, "series": []}},
                    {"id": "map", "kind": "map", "chart": {"series": [{"item": "a"}]}}
                ],
                "active": 0
            },
            "saved": []
        });
        let layout: Layout = serde_json::from_value(stored).unwrap();
        let DockNode::Group { tabs, .. } = &layout.dock else {
            panic!("a group")
        };
        let chart = tabs[0].chart.as_ref().unwrap();
        assert_eq!(chart.span_s, Some(300));
        let series: Vec<(&str, u8, bool)> = chart
            .series
            .iter()
            .map(|s| (s.item.as_str(), s.colour, s.hidden))
            .collect();
        // A repeated colour and one out of range each take the first free.
        assert_eq!(
            series,
            vec![
                ("battery.voltage_v", 3, false),
                ("mavlink.VFR_HUD.climb", 0, true),
                ("battery.current_a", 1, false),
            ]
        );
        assert_eq!(tabs[1].chart.as_ref().unwrap().span_s, None);
        assert_eq!(tabs[2].chart, None);
        let again: Layout = serde_json::from_value(serde_json::to_value(&layout).unwrap()).unwrap();
        assert_eq!(again, layout);
    }

    #[test]
    fn camera_behind_is_kept_on_instruments_tabs_only() {
        let stored = serde_json::json!({
            "paneOpen": true,
            "dock": {
                "type": "group",
                "tabs": [
                    {"id": "instruments", "kind": "instruments", "cameraBehind": true},
                    {"id": "flight-display", "kind": "flight-display", "cameraBehind": true},
                    {"id": "map", "kind": "map", "cameraBehind": true}
                ],
                "active": 0
            },
            "saved": []
        });
        let layout: Layout = serde_json::from_value(stored).unwrap();
        let DockNode::Group { tabs, .. } = &layout.dock else {
            panic!("a group")
        };
        let behind: Vec<bool> = tabs.iter().map(|t| t.camera_behind).collect();
        assert_eq!(behind, vec![true, true, false]);
        let again: Layout = serde_json::from_value(serde_json::to_value(&layout).unwrap()).unwrap();
        assert_eq!(again, layout);
    }

    #[test]
    fn a_map_stored_in_3d_is_a_3d_map_tab_in_its_place() {
        // Before 2026-10-04 a Map tab carried `threeD`; now 3D is a kind of its own.
        let stored = serde_json::json!({
            "paneOpen": true,
            "dock": {
                "type": "group",
                "tabs": [
                    {"id": "map", "kind": "map", "threeD": true, "vehicleId": "v1"},
                    {"id": "t2", "kind": "map"},
                    "mission"
                ],
                "active": 0
            },
            "saved": [{
                "id": "s",
                "name": "Survey",
                "dock": {"type": "group", "tabs": [{"id": "map", "kind": "map", "threeD": true}], "active": 0}
            }]
        });
        let layout: Layout = serde_json::from_value(stored).unwrap();
        let DockNode::Group { tabs, active } = &layout.dock else {
            panic!("a group")
        };
        let kinds: Vec<(&str, TabKind)> = tabs.iter().map(|t| (t.id.as_str(), t.kind)).collect();
        assert_eq!(
            kinds,
            vec![
                ("map", TabKind::Map3d),
                ("t2", TabKind::Map),
                ("mission", TabKind::Mission)
            ]
        );
        assert_eq!(*active, 0);
        assert_eq!(tabs[0].vehicle_id.as_deref(), Some("v1"));
        let DockNode::Group { tabs: saved, .. } = &layout.saved[0].dock else {
            panic!("a group")
        };
        assert_eq!(saved[0].kind, TabKind::Map3d);
        // On the wire the kind is its own, and no map says anything about 3D.
        assert_eq!(
            serde_json::to_value(Tab::of(TabKind::Map3d)).unwrap(),
            serde_json::json!({"id": "map-3d", "kind": "map-3d"})
        );
        assert_eq!(
            serde_json::to_value(Tab::of(TabKind::Map)).unwrap(),
            serde_json::json!({"id": "map", "kind": "map"})
        );
    }

    #[test]
    fn tabs_repeat_and_pin_to_a_vehicle_but_the_chat_is_one() {
        let tab = |id: &str, kind: TabKind, vehicle: Option<&str>| Tab {
            id: id.into(),
            kind,
            vehicle_id: vehicle.map(Into::into),
            hidden: false,
            camera: None,
            panel: None,
            item: None,
            camera_behind: false,
            chart: None,
        };
        let layout = Layout {
            pane_open: true,
            dock: row(vec![
                child(
                    1,
                    DockNode::group(vec![
                        tab("chat", TabKind::Chat, None),
                        tab("t2", TabKind::Chat, None),
                    ]),
                ),
                child(
                    1,
                    DockNode::group(vec![
                        tab("map", TabKind::Map, None),
                        tab("t1", TabKind::Map, Some("v2")),
                        tab("t1", TabKind::Mission, None),
                    ]),
                ),
            ]),
            saved: Vec::new(),
            arranged: false,
            flight_opened: false,
            current: None,
        }
        .normalised();
        assert_eq!(
            layout.dock,
            row(vec![
                child(500, group(&[TabKind::Chat])),
                child(
                    500,
                    DockNode::group(vec![
                        Tab::of(TabKind::Map),
                        tab("t1", TabKind::Map, Some("v2")),
                    ])
                ),
            ])
        );
        let json = serde_json::to_value(&layout.dock).unwrap();
        assert_eq!(json["children"][1]["node"]["tabs"][1]["vehicleId"], "v2");
        assert!(
            json["children"][1]["node"]["tabs"][0]
                .get("vehicleId")
                .is_none()
        );
    }

    #[test]
    fn tabs_stored_as_kinds_are_read() {
        let layout: Layout = serde_json::from_value(serde_json::json!({
            "paneOpen": true,
            "dock": { "type": "group", "tabs": ["chat", "map"], "active": 1 },
            "saved": []
        }))
        .unwrap();
        assert_eq!(
            layout.dock,
            DockNode::Group {
                tabs: vec![Tab::of(TabKind::Chat), Tab::of(TabKind::Map)],
                active: 1
            }
        );
    }

    #[test]
    fn saved_layouts_are_bounded() {
        let saved = |id: &str, name: &str| SavedLayout {
            id: id.into(),
            name: name.into(),
            preset: None,
            dock: group(&[TabKind::Chat]),
        };
        let mut many: Vec<SavedLayout> = (0..30).map(|i| saved(&format!("l{i}"), "x")).collect();
        many.insert(0, saved("l1", "repeated id"));
        many.insert(0, saved("blank", "  "));
        many.insert(0, saved("long", &"x".repeat(41)));
        let layout = Layout {
            saved: many,
            ..Layout::initial()
        }
        .normalised();
        assert_eq!(layout.saved.len(), MAX_SAVED_LAYOUTS);
        assert_eq!(layout.saved[0].id, "l1");
        assert_eq!(layout.saved[0].name, "repeated id");
        assert_eq!(layout.saved[1].id, "l0");
    }

    #[test]
    fn names_are_trimmed_and_bounded() {
        assert_eq!(clean_name("  Ana "), Some("Ana".into()));
        assert_eq!(clean_name("   "), None);
        assert_eq!(clean_name(&"x".repeat(65)), None);
        assert_eq!(
            clean_name(&"é".repeat(64)).map(|n| n.chars().count()),
            Some(64)
        );
    }

    #[test]
    fn a_vehicle_tab_becomes_telemetry_with_control_beside_it() {
        let layout: Layout = serde_json::from_value(serde_json::json!({
            "paneOpen": true,
            "dock": { "type": "split", "direction": "row", "children": [
                { "size": 500, "node": { "type": "group", "tabs": ["chat"], "active": 0 } },
                { "size": 500, "node": { "type": "group", "tabs": [
                    { "id": "map", "kind": "map" },
                    { "id": "vehicle", "kind": "vehicle", "vehicleId": "v1" },
                    { "id": "t9", "kind": "vehicle" }
                ], "active": 1 } }
            ] },
            "saved": []
        }))
        .unwrap();
        let DockNode::Split { children, .. } = &layout.dock else {
            panic!("a split");
        };
        let DockNode::Group { tabs, active } = &children[1].node else {
            panic!("a group");
        };
        let ids: Vec<(&str, TabKind)> = tabs.iter().map(|t| (t.id.as_str(), t.kind)).collect();
        assert_eq!(
            ids,
            [
                ("map", TabKind::Map),
                ("telemetry", TabKind::Telemetry),
                ("control", TabKind::Control),
                ("t9-telemetry", TabKind::Telemetry),
                ("t9-control", TabKind::Control),
            ]
        );
        assert_eq!(*active, 1, "Telemetry showing where the Vehicle tab was");
        assert_eq!(tabs[2].vehicle_id.as_deref(), Some("v1"), "pinned alike");
    }

    #[test]
    fn the_first_run_is_simple_without_home_or_chat() {
        let layout = Layout::initial();
        assert_eq!(
            layout.dock,
            Layout::simple_dock(),
            "with or without a vehicle"
        );
        assert!(layout.pane_open);
        assert_eq!(layout.current.as_deref(), Some("preset-simple"));
        assert!(
            layout.dock.contains(TabKind::Map3d),
            "the 3D map always shows"
        );
        assert!(layout.dock.contains(TabKind::Mission));
        assert!(!layout.dock.contains(TabKind::Inspector));
        assert!(
            !layout.dock.contains(TabKind::Home),
            "Home stays in the UI drawer"
        );
        assert!(
            !layout.dock.contains(TabKind::Chat),
            "the chat opens on request"
        );
    }

    #[test]
    fn an_older_view_migrates_to_the_shipped_baseline() {
        let mut layout = Layout::initial();
        layout.dock = DockNode::of(&[TabKind::Map, TabKind::Camera]);
        layout.saved[0].dock = layout.dock.clone();
        let preset = layout.saved[0].preset.as_mut().unwrap();
        preset.version = LAYOUT_PRESET_VERSION - 1;
        preset.base = Layout::flight_dock();
        let back: Layout = serde_json::from_value(serde_json::to_value(&layout).unwrap()).unwrap();
        assert_eq!(back.dock, Layout::simple_dock());
        assert_eq!(back.saved[0].dock, Layout::simple_dock());
        assert_eq!(
            back.saved[0].preset.as_ref().unwrap().version,
            LAYOUT_PRESET_VERSION
        );
    }

    #[test]
    fn two_presets_do_not_consume_the_custom_layout_limit() {
        let mut layout = Layout::initial();
        layout
            .saved
            .extend((0..MAX_SAVED_LAYOUTS).map(|n| SavedLayout {
                id: format!("custom-{n}"),
                name: format!("Custom {n}"),
                dock: DockNode::of(&[TabKind::Map]),
                preset: None,
            }));
        layout.saved.push(SavedLayout {
            id: "advanced".into(),
            name: "Advanced".into(),
            dock: Layout::flight_dock(),
            preset: Some(LayoutPreset {
                kind: LayoutPresetKind::Advanced,
                version: 1,
                base: Layout::flight_dock(),
            }),
        });
        let mut duplicate = layout.saved[0].clone();
        duplicate.id = "another-simple".into();
        layout.saved.push(duplicate);
        let tidy = layout.normalised();
        assert_eq!(tidy.saved.len(), MAX_SAVED_LAYOUTS + 2);
        assert_eq!(tidy.saved.iter().filter(|s| s.preset.is_some()).count(), 2);
    }

    #[test]
    fn the_shape_ignores_sizes_and_the_showing_tab_but_not_hidden_tabs() {
        let dock = |chat: u16, active: usize, hidden: bool| DockNode::Split {
            direction: Direction::Row,
            children: vec![
                DockChild {
                    size: chat,
                    node: DockNode::of(&[TabKind::Chat]),
                },
                DockChild {
                    size: 1000 - chat,
                    node: DockNode::Group {
                        tabs: vec![
                            Tab::of(TabKind::Map),
                            Tab {
                                hidden,
                                ..Tab::of(TabKind::Telemetry)
                            },
                        ],
                        active,
                    },
                },
            ],
        };
        let a = dock(300, 0, false);
        assert_eq!(a.shape(), dock(100, 1, false).shape());
        assert_ne!(
            a.shape(),
            dock(300, 0, true).shape(),
            "hiding a tab arranges the dock"
        );
    }

    /// The group with the map in the flight layout.
    fn map_group(layout: &Layout) -> DockNode {
        let DockNode::Split { children, .. } = &layout.dock else {
            panic!("the flight layout");
        };
        let DockNode::Split { children: rows, .. } = &children[0].node else {
            panic!("the top row over the bottom one");
        };
        let DockNode::Split { children: top, .. } = &rows[0].node else {
            panic!("the map and the camera");
        };
        top[0].node.clone()
    }

    #[test]
    fn opening_a_tab_shows_the_one_there_or_adds_one_beside_the_map() {
        // None yet: the flight layout opens in the home's place and the tab joins its first group.
        let mut layout = Layout {
            dock: DockNode::of(&[TabKind::Home]),
            ..Layout::initial()
        };
        layout.open(TabKind::Vehicles);
        assert!(layout.pane_open);
        let added = DockNode::Group {
            tabs: vec![Tab::of(TabKind::Map), Tab::of(TabKind::Vehicles)],
            active: 1,
        };
        assert_eq!(map_group(&layout), added);

        // One there, behind another: it shows, and none is added.
        layout.open(TabKind::Map);
        assert!(matches!(
            map_group(&layout),
            DockNode::Group { active: 0, .. }
        ));
        layout.open(TabKind::Vehicles);
        assert_eq!(map_group(&layout), added);

        // The chat alone: the flight layout, the AI kept on its right.
        let mut alone = Layout {
            pane_open: true,
            dock: DockNode::of(&[TabKind::Chat]),
            ..Layout::initial()
        };
        alone.open(TabKind::Vehicles);
        let shape = alone.dock.shape();
        assert!(shape.ends_with(",[chat:chat:])"), "{shape}");
        assert!(alone.dock.contains(TabKind::Vehicles));
        assert!(alone.dock.contains(TabKind::Map));

        // The chat is never opened as a second tab.
        alone.open(TabKind::Chat);
        assert_eq!(alone.dock.shape(), shape);
    }

    #[test]
    fn the_inspector_opens_beside_messages() {
        let mut layout = Layout {
            pane_open: true,
            dock: DockNode::Split {
                direction: Direction::Row,
                children: vec![
                    DockChild {
                        size: 500,
                        node: DockNode::of(&[TabKind::Map]),
                    },
                    DockChild {
                        size: 500,
                        node: DockNode::of(&[TabKind::Messages, TabKind::Parameters]),
                    },
                ],
            },
            ..Layout::initial()
        };
        layout.open(TabKind::Inspector);
        let DockNode::Split { children, .. } = &layout.dock else {
            panic!("the map beside Messages");
        };
        assert_eq!(
            children[1].node,
            DockNode::Group {
                tabs: vec![
                    Tab::of(TabKind::Messages),
                    Tab::of(TabKind::Parameters),
                    Tab::of(TabKind::Inspector),
                ],
                active: 2,
            }
        );

        // Messages beside the chat: nothing joins the chat, so the first other group.
        let mut with_chat = Layout {
            pane_open: true,
            dock: DockNode::Split {
                direction: Direction::Row,
                children: vec![
                    DockChild {
                        size: 500,
                        node: DockNode::of(&[TabKind::Chat, TabKind::Messages]),
                    },
                    DockChild {
                        size: 500,
                        node: DockNode::of(&[TabKind::Map]),
                    },
                ],
            },
            ..Layout::initial()
        };
        with_chat.open(TabKind::Inspector);
        assert_eq!(
            with_chat.dock.shape(),
            "Row([chat:chat:,messages:messages:],[map:map:,inspector:inspector:])"
        );
    }

    #[test]
    fn the_camera_opens_with_parameters() {
        // In the flight layout the camera joins Parameters' group below the
        // maps, so the top row stays the two maps.
        let mut layout = Layout {
            pane_open: true,
            dock: Layout::flight_dock(),
            ..Layout::initial()
        };
        layout.open(TabKind::Camera);
        assert!(layout.dock.shape().contains(
            "[messages:messages:,parameters:parameters:,controllers:controllers:,inspector:inspector:,chart:chart:,camera:camera:]"
        ));
        assert!(layout.dock.shape().contains("[map-3d:map-3d:]"));
        // Without Parameters, the first group without the chat, as any tab.
        let mut flat = Layout {
            pane_open: true,
            dock: DockNode::Split {
                direction: Direction::Row,
                children: vec![
                    DockChild {
                        size: 500,
                        node: DockNode::of(&[TabKind::Chat]),
                    },
                    DockChild {
                        size: 500,
                        node: DockNode::of(&[TabKind::Map]),
                    },
                ],
            },
            ..Layout::initial()
        };
        flat.open(TabKind::Camera);
        assert_eq!(
            flat.dock.shape(),
            "Row([chat:chat:],[map:map:,camera:camera:])"
        );
    }

    #[test]
    fn the_layout_you_are_on_is_kept_and_goes_with_its_saved_layout() {
        // Stored before S66: no current layout.
        let old: Layout = serde_json::from_value(serde_json::json!({
            "paneOpen": true,
            "dock": { "type": "group", "tabs": ["chat"], "active": 0 },
            "saved": [{ "id": "ly1", "name": "Survey", "dock": { "type": "group", "tabs": ["chat"], "active": 0 } }]
        }))
        .unwrap();
        assert_eq!(old.current, None);
        assert!(
            !serde_json::to_string(&old).unwrap().contains("current"),
            "absent when none"
        );

        let on = Layout {
            current: Some("ly1".into()),
            ..old.clone()
        }
        .normalised();
        assert_eq!(on.current.as_deref(), Some("ly1"));
        let back: Layout = serde_json::from_str(&serde_json::to_string(&on).unwrap()).unwrap();
        assert_eq!(back, on, "kept through storage");

        // Its saved layout deleted: no layout is current.
        let gone = Layout {
            saved: Vec::new(),
            ..on
        }
        .normalised();
        assert_eq!(gone.current, None);
        let unknown = Layout {
            current: Some("nope".into()),
            ..old
        }
        .normalised();
        assert_eq!(unknown.current, None);
    }

    #[test]
    fn an_operator_stored_before_s74_has_no_groups_or_pins() {
        let mut stored = serde_json::to_value(Operator {
            id: "op1".into(),
            name: "Ana".into(),
            units: UnitSystem::Metric,
            quantity_units: None,
            coordinate_format: CoordinateFormat::Decimal,
            altitude_frame: crate::domain::telemetry::AltitudeFrame::default(),
            layout: Layout::initial(),
            place_search: true,
            basemap: Basemap::Imagery,
            shortcuts: BTreeMap::new(),
            skipped_confirmations: Vec::new(),
            telemetry_layouts: Vec::new(),
            vehicle_telemetry: BTreeMap::new(),
            parameter_groups: vec![ParameterGroup {
                name: "Hover tuning".into(),
                parameters: vec!["ATC_RAT_RLL_P".into()],
            }],
            inspector_pinned: vec!["received:HEARTBEAT".into()],
            control_actions: vec![ControlActions {
                class: crate::domain::telemetry_layout::VehicleClass::Copter,
                actions: vec!["vehicle:mode:guided".into()],
            }],
            me_marker_id: None,
            display: None,
            account_id: None,
            controller_mappings: Vec::new(),
            version: 1,
        })
        .unwrap();
        assert_eq!(stored["parameterGroups"][0]["name"], "Hover tuning");
        assert_eq!(stored["controlActions"][0]["class"], "copter");
        let fields = stored.as_object_mut().unwrap();
        fields.remove("parameterGroups");
        fields.remove("inspectorPinned");
        fields.remove("controlActions");
        let old: Operator = serde_json::from_value(stored).unwrap();
        assert!(old.parameter_groups.is_empty());
        assert!(old.inspector_pinned.is_empty());
        assert!(old.control_actions.is_empty());
    }

    #[test]
    fn an_operator_saved_with_one_units_choice_reads_it_for_every_quantity() {
        let stored = serde_json::json!({
            "id": "op1", "name": "Ana", "units": "imperial", "coordinateFormat": "decimal",
            "version": 1,
        });
        let mut old: Operator = serde_json::from_value(stored).unwrap();
        assert_eq!(old.quantity_units, None);
        let imperial = old.quantity_units();
        assert_eq!(imperial.altitude, AltitudeUnit::Feet);
        assert_eq!(imperial.speed, SpeedUnit::Knots);
        assert_eq!(imperial.vertical_speed, VerticalSpeedUnit::FeetPerMinute);
        assert_eq!(imperial.temperature, TemperatureUnit::Fahrenheit);
        assert_eq!(imperial.distance, DistanceUnit::Miles);

        // Mixed units keep the old choice as their altitude's system.
        old.set_quantity_units(QuantityUnits {
            altitude: AltitudeUnit::Meters,
            ..imperial
        });
        assert_eq!(old.units, UnitSystem::Metric);
        let saved = serde_json::to_value(&old).unwrap();
        assert_eq!(saved["quantityUnits"]["verticalSpeed"], "feet-per-minute");
        assert_eq!(saved["quantityUnits"]["distance"], "miles");
        let back: Operator = serde_json::from_value(saved).unwrap();
        assert_eq!(back.quantity_units().speed, SpeedUnit::Knots);
        assert_eq!(back.quantity_units().altitude, AltitudeUnit::Meters);
    }

    #[test]
    fn control_actions_are_each_once_sorted_and_bounded() {
        use crate::domain::telemetry_layout::VehicleClass;
        let entry = |class, actions: &[&str]| ControlActions {
            class,
            actions: actions.iter().map(|&a| a.to_owned()).collect(),
        };
        assert_eq!(
            clean_control_actions(&[
                entry(
                    VehicleClass::Copter,
                    &["vehicle:land", "vehicle:mode:guided", "vehicle:land"]
                ),
                entry(VehicleClass::Plane, &[]),
            ]),
            Some(vec![
                entry(VehicleClass::Plane, &[]),
                entry(
                    VehicleClass::Copter,
                    &["vehicle:land", "vehicle:mode:guided"]
                ),
            ])
        );
        for bad in [
            "land",
            "vehicle:",
            "vehicle:Mode:GUIDED",
            "vehicle:mode:guided\n",
        ] {
            assert_eq!(
                clean_control_actions(&[entry(VehicleClass::Copter, &[bad])]),
                None,
                "{bad}"
            );
        }
        let long = format!("vehicle:{}", "a".repeat(60));
        assert_eq!(
            clean_control_actions(&[entry(VehicleClass::Copter, &[&long])]),
            None
        );
        assert_eq!(
            clean_control_actions(&[
                entry(VehicleClass::Copter, &[]),
                entry(VehicleClass::Copter, &[])
            ]),
            None
        );
        let many: Vec<String> = (0..41).map(|i| format!("vehicle:mode:m{i}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        assert_eq!(
            clean_control_actions(&[entry(VehicleClass::Copter, &many)]),
            None
        );
    }

    fn named(name: &str, parameters: &[&str]) -> ParameterGroup {
        ParameterGroup {
            name: name.into(),
            parameters: parameters.iter().map(|&p| p.to_owned()).collect(),
        }
    }

    #[test]
    fn parameter_groups_are_trimmed_sorted_and_bounded() {
        let cleaned = clean_parameter_groups(&[
            named(
                " Hover tuning ",
                &["ATC_RAT_RLL_P", "ATC_RAT_PIT_P", "ATC_RAT_RLL_P"],
            ),
            named("Battery", &["BATT_CAPACITY"]),
        ])
        .unwrap();
        assert_eq!(
            cleaned,
            vec![
                named("Battery", &["BATT_CAPACITY"]),
                named("Hover tuning", &["ATC_RAT_RLL_P", "ATC_RAT_PIT_P"]),
            ]
        );
        for bad in [
            vec![named("  ", &[])],
            vec![named(&"x".repeat(41), &[])],
            vec![named("A", &[]), named(" A", &[])],
            vec![named("A", &["atc_rat_rll_p"])],
            vec![named("A", &["ATC_RAT_RLL_P_TOO_LONG"])],
            vec![named("A", &[""])],
            vec![named("A", &vec!["RTL_ALT"; 201])],
            (0..101).map(|i| named(&i.to_string(), &[])).collect(),
        ] {
            assert_eq!(clean_parameter_groups(&bad), None, "{bad:?}");
        }
    }

    #[test]
    fn inspector_pins_are_a_direction_and_a_message() {
        let pins = |keys: &[&str]| keys.iter().map(|&k| k.to_owned()).collect::<Vec<_>>();
        assert_eq!(
            clean_inspector_pinned(&pins(&[
                "received:HEARTBEAT",
                "sent:COMMAND_LONG",
                "received:HEARTBEAT",
                "received:#12345",
            ])),
            Some(pins(&[
                "received:HEARTBEAT",
                "sent:COMMAND_LONG",
                "received:#12345"
            ]))
        );
        for bad in [
            "HEARTBEAT",
            "heard:HEARTBEAT",
            "received:",
            "received:heartbeat",
            "sent:COMMAND LONG",
        ] {
            assert_eq!(clean_inspector_pinned(&pins(&[bad])), None, "{bad}");
        }
        assert_eq!(
            clean_inspector_pinned(&pins(&[&format!("sent:{}", "A".repeat(65))])),
            None
        );
        assert_eq!(
            clean_inspector_pinned(&vec!["sent:PING".to_owned(); 201]),
            None
        );
    }
}

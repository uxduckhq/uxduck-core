//! Operators: local named profiles on this machine (ADR-0005). Every journal
//! entry names the operator as well as the client, so attribution is right
//! from the first entity. Layouts join in S6, and become a tree in S63.

use std::collections::{BTreeMap, HashSet};

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
    /// Hillshaded relief with contour lines, for the lie of the land.
    Terrain,
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
    /// The focused vehicle's mission, retired for the Missions page
    /// (ADR-0029 P16): tidying drops it from stored layouts.
    Mission,
    /// Gamepads: claims, targets and mappings, retired for the Cockpit's
    /// controllers page (ADR-0005, 2026-11-04): tidying makes it a Cockpit
    /// in its place when the layout has none, and drops it otherwise.
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
    /// The Missions page as a tab: the plans and every vehicle's mission
    /// (ADR-0029 P16).
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
    /// The focused vehicle's tuning (S329, ADR-0054 T1): Feel, the gains
    /// per axis and snapshots, a view of its parameters. It follows the
    /// selected vehicle, or the one it is pinned to.
    Tuning,
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
            Self::Tuning => "tuning",
        }
    }

    /// The kind whose group a new tab of this kind opens in, when there is
    /// one: the inspector beside Messages, the camera and the checklist
    /// beside the Cockpit (the camera below the maps, maintainer, 2026-10-16,
    /// with Parameters until S267 took Parameters out of the built-in
    /// layouts; the checklist where Arm is, S193; Tuning, used in flight,
    /// S329), and Setup beside Parameters (S184).
    /// The web's `JOINS`.
    #[must_use]
    pub const fn joins(self) -> Option<Self> {
        match self {
            Self::Inspector => Some(Self::Messages),
            Self::Camera | Self::Checklist | Self::Tuning => Some(Self::Control),
            Self::Setup => Some(Self::Parameters),
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
    /// How a Camera tab shows its vehicle's cameras (maintainer,
    /// 2026-10-30): one, one large over the others, or all in a grid. None
    /// chooses from how many there are and whether one was chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_view: Option<CameraView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map_camera: Option<MapCameraOptions>,
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
    /// The part a Tuning tab was last asked to show (S333).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<TuningSection>,
}

/// A part of the Tuning tab (S333). Roll, Pitch and Yaw are How it flies
/// on that axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TuningSection {
    Steps,
    Autotune,
    Roll,
    Pitch,
    Yaw,
    Feel,
    Gains,
}

/// How a Camera tab shows its vehicle's cameras.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CameraView {
    /// One camera as large as the tab allows.
    Single,
    /// One camera large, the vehicle's others small under it.
    Focus,
    /// Every camera of the vehicle, the same size, fitted to the tab.
    Grid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MapCameraOptions {
    pub viewpoint: MapViewpoint,
    pub following: bool,
    pub resume: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drag_to_pan: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MapViewpoint {
    Cockpit,
    Rear,
    Tail,
    Orbit,
    Behind,
    Below,
    Camera,
    Home,
    Above,
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
    /// Its slot, which says how its line is drawn: the theme has eight
    /// series colours, so slots 0 to 7 are those as solid lines, 8 to 15
    /// the same colours dashed, then dotted, then dash and dot. Past 31 the
    /// looks repeat.
    #[serde(default)]
    pub colour: u8,
    /// Its line is not drawn.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
}

/// The spans a chart may show, in seconds: what UXDuck keeps is five minutes.
const CHART_SPANS: [u16; 3] = [30, 60, 300];

impl ChartSpec {
    /// Drops series with an empty or overlong item and repeats, however many
    /// are left; gives a series whose slot an earlier one has the first one
    /// free, so each line is drawn its own way while there are slots (a
    /// `u8` has 256, and past them a slot repeats); forgets a span not
    /// offered.
    pub fn tidy(&mut self) {
        let mut seen: HashSet<String> = HashSet::new();
        self.series.retain(|s| {
            !s.item.is_empty() && s.item.len() <= MAX_ID_LEN && seen.insert(s.item.clone())
        });
        let mut used = [false; 256];
        for series in &mut self.series {
            let slot = usize::from(series.colour);
            if !used[slot] {
                used[slot] = true;
                continue;
            }
            if let Some(free) = used.iter().position(|u| !u) {
                used[free] = true;
                series.colour = u8::try_from(free).unwrap_or(series.colour);
            }
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
        camera_view: Option<CameraView>,
        #[serde(default)]
        map_camera: Option<MapCameraOptions>,
        #[serde(default)]
        panel: Option<String>,
        #[serde(default)]
        item: Option<String>,
        #[serde(default)]
        camera_behind: bool,
        #[serde(default)]
        chart: Option<ChartSpec>,
        #[serde(default)]
        section: Option<TuningSection>,
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
                camera_view,
                map_camera,
                panel,
                item,
                camera_behind,
                chart,
                section,
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
                camera_view,
                map_camera,
                panel,
                item,
                camera_behind,
                chart,
                section,
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
            camera_view: None,
            map_camera: None,
            panel: None,
            item: None,
            camera_behind: false,
            chart: None,
            section: None,
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
            camera_view: None,
            map_camera: None,
            panel: None,
            item: None,
            camera_behind: false,
            chart: None,
            section: None,
        })
    }

    /// Drops a pin, camera or panel that is empty or too long, a camera or
    /// camera view on a tab other than a Camera tab, and a panel on one
    /// other than a Plugin tab.
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
        if self.kind != TabKind::Camera {
            self.camera_view = None;
        }
        if self.kind != TabKind::Map3d {
            self.map_camera = None;
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
        if self.kind != TabKind::Tuning {
            self.section = None;
        }
    }
}

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
/// 13 (2026-10-25): no Mission tab; the Missions page took its place.
/// 14 (2026-11-01, S267): Simple is the two maps above the Cockpit and
/// Controllers; Advanced leaves out vehicle setup's tools and the camera.
/// 15 (2026-11-04): no Controllers tab; the controllers are in the Cockpit.
pub const LAYOUT_PRESET_VERSION: u32 = 15;

/// The longest saved layout name, in characters.
pub const MAX_LAYOUT_NAME_CHARS: usize = 40;
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
    /// group shows, and shows it if hidden. Its id, when there was one.
    fn show(&mut self, kind: TabKind) -> Option<String> {
        let tabs = self.tabs();
        let of_kind: Vec<&&Tab> = tabs.iter().filter(|t| t.kind == kind).collect();
        let target = of_kind
            .iter()
            .find(|t| t.vehicle_id.is_none() && !t.hidden)
            .or_else(|| of_kind.iter().find(|t| !t.hidden))
            .or_else(|| of_kind.first())
            .map(|t| t.id.clone())?;
        self.show_id(&target);
        Some(target)
    }

    /// Asks the tab with this id to show a part of it (S333).
    fn set_section(&mut self, id: &str, section: TuningSection) {
        match self {
            Self::Group { tabs, .. } => {
                if let Some(tab) = tabs.iter_mut().find(|t| t.id == id) {
                    tab.section = Some(section);
                }
            }
            Self::Split { children, .. } => {
                for child in children {
                    child.node.set_section(id, section);
                }
            }
        }
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

    /// Adds a tab, showing, to the first group that takes it
    /// ([`takes_new`]). Whether there was one.
    fn add_to_first_group(&mut self, tab: &Tab) -> bool {
        match self {
            Self::Group { tabs, active } => {
                if !takes_new(tabs) {
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

    /// Adds a tab to the first group holding a tab of `kind` that takes it
    /// ([`takes_new`]), showing it. Whether there was one.
    fn add_to_group_with(&mut self, kind: TabKind, tab: &Tab) -> bool {
        match self {
            Self::Group { tabs, active } => {
                if !tabs.iter().any(|t| t.kind == kind) || !takes_new(tabs) {
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

    /// Makes the first tab of kind `from` one of kind `to`, in its place.
    fn retype_first(&mut self, from: TabKind, to: TabKind) -> bool {
        match self {
            Self::Group { tabs, .. } => tabs
                .iter_mut()
                .find(|t| t.kind == from)
                .map(|tab| tab.kind = to)
                .is_some(),
            Self::Split { children, .. } => children
                .iter_mut()
                .any(|child| child.node.retype_first(from, to)),
        }
    }

    /// Puts a tab in a group of its own beside the first group showing a
    /// map: after it, at half its share, when that group is in a row; else
    /// in a row with it, in its place. Whether there was a map. The web's
    /// `besideMap`.
    fn add_beside_map(&mut self, tab: &Tab) -> bool {
        if shows_map(self) {
            let map = std::mem::replace(self, Self::group(Vec::new()));
            *self = Self::Split {
                direction: Direction::Row,
                children: vec![
                    DockChild {
                        size: SIZE_TOTAL / 2,
                        node: map,
                    },
                    DockChild {
                        size: SIZE_TOTAL / 2,
                        node: Self::group(vec![tab.clone()]),
                    },
                ],
            };
            return true;
        }
        let Self::Split {
            direction,
            children,
        } = self
        else {
            return false;
        };
        let Some(at) = children.iter().position(|c| shows_map(&c.node)) else {
            return children.iter_mut().any(|c| c.node.add_beside_map(tab));
        };
        if *direction == Direction::Row {
            let half = children[at].size / 2;
            children[at].size -= half;
            children.insert(
                at + 1,
                DockChild {
                    size: half,
                    node: Self::group(vec![tab.clone()]),
                },
            );
            return true;
        }
        children[at].node.add_beside_map(tab)
    }

    /// Whether a tab of this kind is anywhere in this part.
    #[must_use]
    pub fn contains(&self, kind: TabKind) -> bool {
        match self {
            Self::Group { tabs, .. } => tabs.iter().any(|t| t.kind == kind),
            Self::Split { children, .. } => children.iter().any(|c| c.node.contains(kind)),
        }
    }

    /// Drops repeated tabs (by id, and any chat after the first) and empty
    /// parts, folds a split of one child into it and a split into a parent
    /// going the same way, and scales sizes so a split's add up to
    /// `SIZE_TOTAL`. However many tabs there are, each is kept. `None` if
    /// nothing is left.
    fn tidy(self, seen: &mut HashSet<String>) -> Option<Self> {
        match self {
            Self::Group { tabs, active } => {
                let mut shown = tabs.get(active).map(|t| t.id.clone());
                // A Vehicle tab is Telemetry with Cockpit beside it, Telemetry showing.
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
                    // Get started is in the chat now, a vehicle's mission in the
                    // Missions page (ADR-0029 P16) and controllers in the
                    // Cockpit (ADR-0005, 2026-11-04): their old tabs go.
                    if matches!(
                        tab.kind,
                        TabKind::GetStarted | TabKind::Mission | TabKind::Controllers
                    ) {
                        continue;
                    }
                    let chat_again =
                        tab.kind == TabKind::Chat && seen.contains(TabKind::Chat.slug());
                    if tab.id.is_empty()
                        || tab.id.len() > MAX_ID_LEN
                        || seen.contains(&tab.id)
                        || chat_again
                    {
                        continue;
                    }
                    // The chat is one tab, always under its own name.
                    if tab.kind == TabKind::Chat {
                        TabKind::Chat.slug().clone_into(&mut tab.id);
                        tab.vehicle_id = None;
                    }
                    tab.drop_bad_ids();
                    seen.insert(tab.id.clone());
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

/// Sizes in proportion, adding up to `SIZE_TOTAL`, none below one. A split
/// of more children than `SIZE_TOTAL` cannot do both: each of its children
/// gets one, and the sizes add up to their number.
fn scaled(children: Vec<(u32, DockNode)>) -> Vec<DockChild> {
    let total: u64 = children
        .iter()
        .map(|(size, _)| u64::from(*size))
        .sum::<u64>()
        .max(1);
    let count = u64::try_from(children.len()).unwrap_or(u64::MAX);
    let total_size = u64::from(SIZE_TOTAL);
    let mut left = total_size;
    let last = children.len().saturating_sub(1);
    children
        .into_iter()
        .enumerate()
        .map(|(i, (size, node))| {
            let rest = count
                .saturating_sub(u64::try_from(i).unwrap_or(u64::MAX))
                .saturating_sub(1);
            let size = if i == last {
                left.max(1)
            } else {
                (u64::from(size) * total_size / total).clamp(1, left.saturating_sub(rest).max(1))
            };
            left = left.saturating_sub(size);
            DockChild {
                size: u16::try_from(size).unwrap_or(SIZE_TOTAL),
                node,
            }
        })
        .collect()
}

/// Whether a group takes a new tab: none with the chat, and none showing a
/// map, so a tab opened from the palette, a menu or the AI never covers the
/// map (FB-95, the maintainer, 2026-11-04). The web's `takesNew`.
fn takes_new(tabs: &[Tab]) -> bool {
    !tabs.iter().any(|t| {
        t.kind == TabKind::Chat || (matches!(t.kind, TabKind::Map | TabKind::Map3d) && !t.hidden)
    })
}

/// Whether this part is a group showing a flat or 3D map.
fn shows_map(node: &DockNode) -> bool {
    matches!(node, DockNode::Group { tabs, .. } if tabs.iter().any(|t| matches!(t.kind, TabKind::Map | TabKind::Map3d) && !t.hidden))
}

/// A tidy tree. The chat may be left out: the person closed it, and a new
/// chat brings it back (maintainer, 2026-10-02).
fn normalised_dock(mut dock: DockNode) -> DockNode {
    // Without a Cockpit, a Controllers tab becomes one in its place, so its
    // controllers stay where they were reached.
    if !dock.contains(TabKind::Control) {
        dock.retype_first(TabKind::Controllers, TabKind::Control);
    }
    let mut seen = HashSet::new();
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

    /// The map and the 3D map (always, maintainer 2026-10-18) side by side
    /// above the Cockpit, which holds the controllers (ADR-0005,
    /// 2026-11-04), and nothing else (maintainer, 2026-11-01, S267): every other tool is in the
    /// add-panel menu, and the UI adds a Camera tab when a vehicle has a
    /// camera set up.
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
                    node: DockNode::of(&[TabKind::Control]),
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

    /// A flight tool's layout, lean for pros (maintainer, 2026-11-01, S267):
    /// the map and the 3D map side by side on top; below, the Cockpit (with
    /// the controllers), Instruments with the classic instruments, and Telemetry
    /// with Messages. Vehicle setup's tools (Parameters, Files, the MAVLink
    /// inspector, History) are in the add-panel menu and the vehicle's page,
    /// and the UI adds a Camera tab when a vehicle has a camera set up. No
    /// chat: it opens on request as the left column (maintainer,
    /// 2026-10-05). "Reset layout" brings it back.
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
            Direction::Column,
            vec![
                (
                    580,
                    split(
                        Direction::Row,
                        vec![
                            (500, DockNode::of(&[TabKind::Map])),
                            (500, DockNode::of(&[TabKind::Map3d])),
                        ],
                    ),
                ),
                (
                    420,
                    split(
                        Direction::Row,
                        vec![
                            (380, DockNode::of(&[TabKind::Control])),
                            (
                                320,
                                DockNode::of(&[TabKind::FlightDisplay, TabKind::Instruments]),
                            ),
                            (300, DockNode::of(&[TabKind::Telemetry, TabKind::Messages])),
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
    /// group of the kind it joins (the inspector beside Messages, the camera with the Cockpit), the
    /// first group without the chat or a map, beside the map, or beside
    /// everything. Opens the pane.
    pub fn open(&mut self, kind: TabKind) {
        self.open_tab(Tab::of(kind));
    }

    /// Shows a tab as [`Self::open`] does; one given a chart or an item is
    /// always a new one, shown with what it was given (S43), and one given
    /// only a vehicle is the tab of the kind pinned to it, else a new one.
    pub fn open_tab(&mut self, tab: Tab) {
        let kind = tab.kind;
        if kind == TabKind::Chat {
            return;
        }
        // From the home alone, or the chat: the flight layout.
        if !self.dock.has_more_than_home() {
            self.dock = Self::flight_dock_keeping_chat(&self.dock);
        }
        self.pane_open = true;
        let drawn = tab.chart.is_some() || tab.item.is_some();
        if !drawn
            && tab.vehicle_id.is_none()
            && let Some(id) = self.dock.show(kind)
        {
            if let Some(section) = tab.section {
                self.dock.set_section(&id, section);
            }
            return;
        }
        // Asked again for a vehicle's tab: the one pinned to it, not another.
        let pinned = (!drawn && tab.vehicle_id.is_some())
            .then(|| {
                self.dock
                    .tabs()
                    .into_iter()
                    .find(|t| t.kind == kind && t.vehicle_id == tab.vehicle_id)
                    .map(|t| t.id.clone())
            })
            .flatten();
        if let Some(id) = pinned {
            self.dock.show_id(&id);
            if let Some(section) = tab.section {
                self.dock.set_section(&id, section);
            }
            return;
        }
        let mut id = kind.slug().to_owned();
        let mut n = 2;
        while self.dock.has_tab(&id) {
            id = format!("{}-{n}", kind.slug());
            n += 1;
        }
        let tab = Tab { id, ..tab };
        let joined = kind
            .joins()
            .is_some_and(|with| self.dock.add_to_group_with(with, &tab));
        if !joined && !self.dock.add_to_first_group(&tab) && !self.dock.add_beside_map(&tab) {
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

    /// Tidies the tree and the saved layouts, so a stored layout always
    /// renders. Every saved layout with a name and an id of its own is
    /// kept, however many there are and however long its name: a name's
    /// length is checked where a person gives it (`clean_layout_name`), so
    /// that one too long is refused with words there and never dropped here.
    #[must_use]
    pub fn normalised(mut self) -> Self {
        // Saved names are user-owned; built-ins follow each shipped revision.
        // A built-in whose default did not change keeps its arrangement.
        for saved in &mut self.saved {
            if let Some(preset) = &mut saved.preset
                && preset.version > 0
                && preset.version < LAYOUT_PRESET_VERSION
            {
                let dock = match preset.kind {
                    LayoutPresetKind::Simple => Self::simple_dock(),
                    LayoutPresetKind::Advanced => Self::flight_dock(),
                };
                preset.version = LAYOUT_PRESET_VERSION;
                if preset.base == dock {
                    continue;
                }
                saved.dock = dock.clone();
                preset.base = dock.clone();
                if self.current.as_ref() == Some(&saved.id) {
                    self.dock = dock;
                    self.pane_open = true;
                }
            }
        }
        self.dock = normalised_dock(self.dock);
        let mut ids: HashSet<String> = HashSet::new();
        let mut presets: Vec<LayoutPresetKind> = Vec::new();
        self.saved = std::mem::take(&mut self.saved)
            .into_iter()
            .filter_map(|saved| {
                let name = saved.name.trim();
                if name.is_empty() || saved.id.is_empty() || ids.contains(&saved.id) {
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
                    None
                };
                ids.insert(saved.id.clone());
                Some(SavedLayout {
                    name: name.to_owned(),
                    id: saved.id,
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
    /// Values they work out from telemetry with a formula (ADR-0056),
    /// which the daemon adds to every vehicle's telemetry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calculated_items: Vec<crate::domain::calculated::CalculatedItem>,
    /// Groups of parameters the operator tunes together, such as Hover
    /// tuning (S74), by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameter_groups: Vec<ParameterGroup>,
    /// MAVLink messages the operator pinned to the top of the inspector, as
    /// `received:HEARTBEAT` or `sent:COMMAND_LONG`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inspector_pinned: Vec<String>,
    /// The actions the Cockpit tab shows as buttons, in the operator's
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
    /// The Terms of Service they last agreed to on this computer
    /// (ADR-0047), recorded by UXDuck when they agree and never set by a
    /// client; synced with the operator. UXDuck arms a real vehicle for
    /// them only once it is the current version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terms: Option<TermsAcceptance>,
    /// Bumped on every change; writes carry the version they started from.
    pub version: u64,
}

/// An operator's agreement to the Terms of Service (ADR-0047).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TermsAcceptance {
    /// `TERMS_VERSION` as it was when they agreed.
    pub version: String,
    pub accepted_at_ms: u64,
    /// The account signed in on this computer when they agreed, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
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

/// The longest parameter group name, in characters.
pub const MAX_GROUP_NAME_CHARS: usize = 40;
/// The longest parameter name, in characters, as in ArduPilot.
pub const MAX_PARAMETER_NAME_CHARS: usize = 16;

/// Parameter groups with their names trimmed, sorted by name, and each
/// parameter once, however many groups there are and however many
/// parameters in each; `None` if a name is empty or too long, a parameter
/// is not a parameter's name, or two groups share a name.
#[must_use]
pub fn clean_parameter_groups(groups: &[ParameterGroup]) -> Option<Vec<ParameterGroup>> {
    let parameter = |name: &String| {
        (1..=MAX_PARAMETER_NAME_CHARS).contains(&name.len())
            && name
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    };
    let mut names: HashSet<&str> = HashSet::with_capacity(groups.len());
    let mut kept: Vec<ParameterGroup> = Vec::with_capacity(groups.len());
    for group in groups {
        let name = group.name.trim();
        if name.is_empty()
            || name.chars().count() > MAX_GROUP_NAME_CHARS
            || !group.parameters.iter().all(parameter)
            || !names.insert(name)
        {
            return None;
        }
        let mut seen: HashSet<&str> = HashSet::with_capacity(group.parameters.len());
        kept.push(ParameterGroup {
            name: name.to_owned(),
            parameters: group
                .parameters
                .iter()
                .filter(|p| seen.insert(p.as_str()))
                .cloned()
                .collect(),
        });
    }
    kept.sort_by(|a, b| a.name.cmp(&b.name));
    Some(kept)
}

/// The longest message name in a pin, in characters.
pub const MAX_PINNED_MESSAGE_CHARS: usize = 64;

/// Pinned inspector messages, each once in the order given, however many
/// there are; `None` if any is not `received:` or `sent:` and a message
/// name.
#[must_use]
pub fn clean_inspector_pinned(pinned: &[String]) -> Option<Vec<String>> {
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
    let mut seen: HashSet<&str> = HashSet::with_capacity(pinned.len());
    let mut kept: Vec<String> = Vec::with_capacity(pinned.len());
    for key in pinned {
        if !valid(key) {
            return None;
        }
        if seen.insert(key) {
            kept.push(key.clone());
        }
    }
    Some(kept)
}

/// The Cockpit tab's buttons for one type of vehicle, in order, by action
/// id such as `vehicle:arm` or `vehicle:mode:guided`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlActions {
    pub class: crate::domain::telemetry_layout::VehicleClass,
    pub actions: Vec<String>,
}

/// The mapping an operator uses with one controller on one vehicle: by the
/// controller's id on the computer where it was chosen, and its name, which
/// finds the same kind of controller on another computer. The vehicle's id
/// is the same on every computer that knows its autopilot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControllerMapping {
    pub controller_id: String,
    pub controller_name: String,
    /// None for one chosen before any vehicle was, or kept from before
    /// mappings were kept per vehicle (maintainer, 2026-11-12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    pub mapping_id: String,
    /// Set once the controller's id is gone for good (maintainer,
    /// 2026-11-15): a controller with no serial number has a new id each
    /// time it is plugged in. Only the computer it was plugged into can
    /// tell, so only that computer sets this. No lookup reads it: the entry
    /// is found by its name as before, until
    /// [`Operator::retire_controller`] drops it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retired: bool,
}

impl Operator {
    /// The mapping this operator chose for a controller on a vehicle, known
    /// by any of its ids: for that controller, else one of the same name.
    #[must_use]
    pub fn vehicle_mapping(
        &self,
        controller_id: &str,
        controller_name: &str,
        vehicle_ids: &[&str],
    ) -> Option<&str> {
        let on = |m: &&ControllerMapping| {
            m.vehicle_id
                .as_deref()
                .is_some_and(|id| vehicle_ids.contains(&id))
        };
        let newest = self.controller_mappings.iter().rev();
        let known = self
            .controller_mappings
            .iter()
            .any(|m| m.controller_id == controller_id);
        newest
            .clone()
            .filter(on)
            .find(|m| m.controller_id == controller_id)
            .or_else(|| {
                newest
                    .filter(on)
                    .find(|m| !known && m.controller_name == controller_name)
            })
            .map(|m| m.mapping_id.as_str())
    }

    /// The mapping this operator uses with a controller on a vehicle: the
    /// one chosen for it, else the one chosen last for that controller, else
    /// for one of the same name.
    #[must_use]
    pub fn mapping_for(
        &self,
        controller_id: &str,
        controller_name: &str,
        vehicle_ids: &[&str],
    ) -> Option<&str> {
        self.vehicle_mapping(controller_id, controller_name, vehicle_ids)
            .or_else(|| {
                let mut newest = self.controller_mappings.iter().rev();
                newest
                    .clone()
                    .find(|m| m.controller_id == controller_id)
                    .or_else(|| newest.find(|m| m.controller_name == controller_name))
                    .map(|m| m.mapping_id.as_str())
            })
    }

    /// Notes the mapping chosen for a controller on a vehicle, or with none
    /// yet, newest last; `false` if it was already the one. Every pair of a
    /// controller and a vehicle keeps its own, however many there are: none
    /// is dropped to make room for a newer one.
    pub fn choose_mapping(
        &mut self,
        controller_id: &str,
        controller_name: &str,
        vehicle_id: Option<&str>,
        mapping_id: &str,
    ) -> bool {
        let same = |m: &ControllerMapping| {
            m.controller_id == controller_id && m.vehicle_id.as_deref() == vehicle_id
        };
        if self
            .controller_mappings
            .iter()
            .any(|m| same(m) && m.mapping_id == mapping_id)
        {
            return false;
        }
        self.controller_mappings.retain(|m| !same(m));
        self.controller_mappings.push(ControllerMapping {
            controller_id: controller_id.to_owned(),
            controller_name: controller_name.to_owned(),
            vehicle_id: vehicle_id.map(str::to_owned),
            mapping_id: mapping_id.to_owned(),
            retired: false,
        });
        true
    }

    /// Marks what is kept for a controller whose id is gone for good, then
    /// drops each retired mapping that has a newer retired one of the same
    /// name, vehicle and mapping, whichever computer marked it. `false` if
    /// nothing changed.
    ///
    /// No controller is asked about by a retired id again, and a lookup by
    /// name takes the newest of a name and vehicle, so [`Self::vehicle_mapping`]
    /// and [`Self::mapping_for`] answer for every other id as they did. The
    /// newer one has to be retired too, and of the same mapping, because
    /// resetting a controller removes what is kept for its id and deleting a
    /// mapping removes what names it: either would otherwise bring back an
    /// older choice that was dropped here.
    pub fn retire_controller(&mut self, controller_id: &str) -> bool {
        let mut changed = false;
        for mapping in &mut self.controller_mappings {
            if mapping.controller_id == controller_id && !mapping.retired {
                mapping.retired = true;
                changed = true;
            }
        }
        let mut newer = HashSet::new();
        let mut kept: Vec<bool> = self
            .controller_mappings
            .iter()
            .rev()
            .map(|m| !m.retired || newer.insert((&m.controller_name, &m.vehicle_id, &m.mapping_id)))
            .collect();
        changed |= kept.contains(&false);
        self.controller_mappings
            .retain(|_| kept.pop() == Some(true));
        changed
    }
}

/// The longest action id on the Cockpit tab, in characters.
pub const MAX_CONTROL_ACTION_CHARS: usize = 64;

/// Each type once, sorted, with each action once in the order given,
/// however many buttons a type has; `None` if a type comes twice or an id
/// is not `vehicle:` and lower-case letters, digits, `-`, `_`, `:` or
/// spaces (PX4's `Precision Land`).
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
        if !entry.actions.iter().all(valid) || kept.iter().any(|k| k.class == entry.class) {
            return None;
        }
        let mut seen: HashSet<&str> = HashSet::with_capacity(entry.actions.len());
        kept.push(ControlActions {
            class: entry.class,
            actions: entry
                .actions
                .iter()
                .filter(|id| seen.insert(id.as_str()))
                .cloned()
                .collect(),
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
                { "tabs": ["map", "history"], "active": 1 },
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
                        tabs: vec![Tab::of(TabKind::Map), Tab::of(TabKind::History)],
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
        let top = &flight["children"][0]["node"]["children"];
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
                                children: vec![child(5, group(&[TabKind::History]))],
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
                child(250, group(&[TabKind::History])),
                child(
                    250,
                    DockNode::Group {
                        tabs: vec![Tab {
                            id: "3d".into(),
                            kind: TabKind::Map3d,
                            vehicle_id: None,
                            hidden: false,
                            camera: None,
                            camera_view: None,
                            map_camera: None,
                            panel: None,
                            item: None,
                            camera_behind: false,
                            chart: None,
                            section: None,
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
                            {"item": "battery.current_a", "colour": 12},
                            {"item": "speed.ground_mps", "colour": 12}
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
        // A repeated slot takes the first free. Slot 12 is the fifth colour
        // dashed, and is kept by the first series that has it.
        assert_eq!(
            series,
            vec![
                ("battery.voltage_v", 3, false),
                ("mavlink.VFR_HUD.climb", 0, true),
                ("battery.current_a", 12, false),
                ("speed.ground_mps", 1, false),
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
    fn camera_view_is_kept_on_camera_tabs_only() {
        let stored = serde_json::json!({
            "paneOpen": true,
            "dock": {
                "type": "group",
                "tabs": [
                    {"id": "camera", "kind": "camera", "cameraView": "grid"},
                    {"id": "camera-2", "kind": "camera", "camera": "front", "cameraView": "single"},
                    {"id": "map", "kind": "map", "cameraView": "focus"}
                ],
                "active": 0
            },
            "saved": []
        });
        let layout: Layout = serde_json::from_value(stored).unwrap();
        let DockNode::Group { tabs, .. } = &layout.dock else {
            panic!("a group")
        };
        let views: Vec<Option<CameraView>> = tabs.iter().map(|t| t.camera_view).collect();
        assert_eq!(
            views,
            vec![Some(CameraView::Grid), Some(CameraView::Single), None]
        );
        let again: Layout = serde_json::from_value(serde_json::to_value(&layout).unwrap()).unwrap();
        assert_eq!(again, layout);
    }

    #[test]
    fn map_cameras_are_kept_on_their_own_3d_tabs_only() {
        let options =
            serde_json::json!({ "viewpoint": "behind", "following": false, "resume": true });
        let stored = serde_json::json!({
            "paneOpen": true, "dock": { "type": "group", "active": 0, "tabs": [
                { "id": "one", "kind": "map-3d", "mapCamera": options },
                { "id": "two", "kind": "map-3d" },
                { "id": "flat", "kind": "map", "mapCamera": options }
            ] }, "saved": []
        });
        let layout: Layout = serde_json::from_value(stored).unwrap();
        let DockNode::Group { tabs, .. } = &layout.dock else {
            panic!("a group")
        };
        assert_eq!(
            tabs[0].map_camera,
            Some(MapCameraOptions {
                viewpoint: MapViewpoint::Behind,
                following: false,
                resume: true,
                drag_to_pan: None,
            })
        );
        assert!(tabs[1].map_camera.is_none() && tabs[2].map_camera.is_none());
        let restored: Layout =
            serde_json::from_value(serde_json::to_value(&layout).unwrap()).unwrap();
        assert_eq!(restored, layout);
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
                    "history"
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
                ("history", TabKind::History)
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
            camera_view: None,
            map_camera: None,
            panel: None,
            item: None,
            camera_behind: false,
            chart: None,
            section: None,
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
                        tab("t1", TabKind::History, None),
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

    /// 20 was the most until 2026-11-14, and the 21st was dropped without a
    /// word, as was one whose name was over 40 characters.
    #[test]
    fn every_saved_layout_is_kept_however_many_and_whatever_its_names_length() {
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
        // The long name and all thirty ids. Only the one with no name to
        // show and the second with the id l1 go.
        assert_eq!(layout.saved.len(), 31);
        assert_eq!(layout.saved[0].id, "long");
        assert_eq!(layout.saved[0].name, "x".repeat(41));
        assert_eq!(layout.saved[1].id, "l1");
        assert_eq!(layout.saved[1].name, "repeated id");
        assert_eq!(layout.saved[2].id, "l0");
        assert_eq!(layout.saved[30].id, "l29");
        // The name's length is said where a person gives the name.
        assert_eq!(clean_layout_name(&"x".repeat(41)), None);
        assert_eq!(clean_layout_name(&"x".repeat(40)), Some("x".repeat(40)));
    }

    /// 32 was the most until 2026-11-14, and the 33rd was dropped without
    /// a word.
    #[test]
    fn every_tab_is_kept_however_many() {
        let tabs: Vec<Tab> = (0..100)
            .map(|n| Tab {
                id: format!("history-{n}"),
                ..Tab::of(TabKind::History)
            })
            .collect();
        let layout = Layout {
            dock: row(vec![
                child(1, DockNode::group(tabs[..60].to_vec())),
                child(1, DockNode::group(tabs[60..].to_vec())),
            ]),
            ..Layout::initial()
        }
        .normalised();
        let kept: Vec<&str> = layout.dock.tabs().iter().map(|t| t.id.as_str()).collect();
        assert_eq!(kept.len(), 100);
        assert_eq!(kept[32], "history-32");
        assert_eq!(kept[99], "history-99");
    }

    /// With no most tabs, a split may have more children than its sizes
    /// add up to. It is still tidied, each child with a size.
    #[test]
    fn a_split_of_more_parts_than_the_size_total_keeps_them_all() {
        let children: Vec<DockChild> = (0..1500)
            .map(|n| {
                child(
                    u16::MAX,
                    DockNode::group(vec![Tab {
                        id: format!("history-{n}"),
                        ..Tab::of(TabKind::History)
                    }]),
                )
            })
            .collect();
        let layout = Layout {
            dock: row(children),
            ..Layout::initial()
        }
        .normalised();
        let DockNode::Split { children, .. } = &layout.dock else {
            panic!("a split")
        };
        assert_eq!(children.len(), 1500);
        assert!(children.iter().all(|c| c.size >= 1));
    }

    /// Eight was the most until 2026-11-14, one for each colour.
    #[test]
    fn a_chart_keeps_every_series_each_with_a_slot_of_its_own() {
        let mut chart = ChartSpec {
            series: (0..20)
                .map(|n| ChartSeries {
                    item: format!("mavlink.RC_CHANNELS.chan{n}_raw"),
                    colour: 0,
                    hidden: false,
                })
                .collect(),
            span_s: None,
        };
        chart.tidy();
        assert_eq!(chart.series.len(), 20);
        let slots: Vec<u8> = chart.series.iter().map(|s| s.colour).collect();
        assert_eq!(slots, (0..20).collect::<Vec<u8>>());
        // Past every slot a `u8` has, a line shares one. None is dropped.
        let mut crowded = ChartSpec {
            series: (0..300)
                .map(|n| ChartSeries {
                    item: format!("item.n{n}"),
                    colour: 0,
                    hidden: false,
                })
                .collect(),
            span_s: None,
        };
        crowded.tidy();
        assert_eq!(crowded.series.len(), 300);
        assert_eq!(crowded.series[255].colour, 255);
    }

    /// 128 pairs were the most until 2026-11-14, and the oldest choice went
    /// without a word when the 129th was made.
    #[test]
    fn every_controller_mapping_is_kept_however_many() {
        let mut operator = operator_with_no_mappings();
        for n in 0..200 {
            assert!(operator.choose_mapping(
                &format!("unit-{n}"),
                "Radio",
                Some("v1"),
                &format!("mapping-{n}"),
            ));
        }
        assert_eq!(operator.controller_mappings.len(), 200);
        assert_eq!(
            operator.vehicle_mapping("unit-0", "Radio", &["v1"]),
            Some("mapping-0")
        );
        assert_eq!(
            operator.vehicle_mapping("unit-199", "Radio", &["v1"]),
            Some("mapping-199")
        );
    }

    /// A controller with no serial number has a new id each time it is
    /// plugged in, so what was kept for each of them piled up (maintainer,
    /// 2026-11-15).
    #[test]
    fn a_retired_mapping_goes_once_a_newer_retired_one_stands_for_it() {
        let mut operator = operator_with_no_mappings();
        operator.choose_mapping("unit-1", "Radio", Some("v1"), "mode-1");
        operator.choose_mapping("keyboard:1", "Keyboard", Some("v1"), "sim-keyboard");
        operator.choose_mapping("gp-session-1", "Radio", Some("v1"), "mode-2");
        assert!(operator.retire_controller("gp-session-1"));
        assert!(!operator.retire_controller("gp-session-1"), "marked once");
        assert_eq!(operator.controller_mappings.len(), 3, "the newest stays");

        // The next one plugged in is found by its name, and while it is in
        // use the one before it stays: resetting it would bring that back.
        assert_eq!(
            operator.mapping_for("gp-session-2", "Radio", &["v1"]),
            Some("mode-2")
        );
        operator.choose_mapping("gp-session-2", "Radio", Some("v1"), "mode-2");
        assert_eq!(operator.controller_mappings.len(), 4);
        assert!(operator.retire_controller("gp-session-2"));
        let ids = |operator: &Operator| -> Vec<String> {
            operator
                .controller_mappings
                .iter()
                .map(|m| m.controller_id.clone())
                .collect()
        };
        assert_eq!(ids(&operator), ["unit-1", "keyboard:1", "gp-session-2"]);
        assert_eq!(
            operator.mapping_for("gp-session-3", "Radio", &["v1"]),
            Some("mode-2")
        );

        // Another mapping, another vehicle and another name are each kept:
        // deleting a mapping brings back the choice before it.
        operator.choose_mapping("gp-session-3", "Radio", Some("v1"), "mode-1");
        operator.choose_mapping("gp-session-3", "Radio", Some("v2"), "mode-2");
        operator.choose_mapping("gp-session-3", "Radio", None, "mode-2");
        operator.choose_mapping("gp-session-4", "Other radio", Some("v1"), "mode-2");
        assert!(operator.retire_controller("gp-session-3"));
        assert!(operator.retire_controller("gp-session-4"));
        assert_eq!(operator.controller_mappings.len(), 7);
        assert!(
            operator
                .controller_mappings
                .iter()
                .all(|m| m.retired == m.controller_id.starts_with("gp-session-"))
        );
    }

    /// A document from before `retired` reads as it did and is written back
    /// alike, so a computer that does not know the field still takes it.
    /// What ended before the field was kept is never marked, so it stays.
    #[test]
    fn mappings_from_before_they_were_retired_read_and_stay_as_they_were() {
        let stored = serde_json::json!({
            "id": "op1",
            "name": "Ana",
            "units": "metric",
            "coordinateFormat": "decimal",
            "controllerMappings": [
                { "controllerId": "gp-session-old-1", "controllerName": "Radio", "vehicleId": "v1", "mappingId": "mode-1" },
                { "controllerId": "gp-session-old-2", "controllerName": "Radio", "vehicleId": "v1", "mappingId": "mode-1" },
                { "controllerId": "gp-session-old-3", "controllerName": "Radio", "mappingId": "mode-2" },
            ],
            "version": 7
        });
        let mut operator: Operator = serde_json::from_value(stored.clone()).unwrap();
        assert!(operator.controller_mappings.iter().all(|m| !m.retired));
        let written = serde_json::to_value(&operator).unwrap();
        assert_eq!(
            written["controllerMappings"], stored["controllerMappings"],
            "no new key in what was not retired"
        );

        operator.choose_mapping("gp-session-new", "Radio", Some("v1"), "mode-1");
        assert!(operator.retire_controller("gp-session-new"));
        assert_eq!(operator.controller_mappings.len(), 4);
        let written = serde_json::to_value(&operator).unwrap();
        assert_eq!(
            written["controllerMappings"][3],
            serde_json::json!({ "controllerId": "gp-session-new", "controllerName": "Radio", "vehicleId": "v1", "mappingId": "mode-1", "retired": true }),
        );
        assert_eq!(
            serde_json::from_value::<Operator>(written).unwrap(),
            operator
        );
    }

    /// Stick mappings decide stick modes, so retiring must never change
    /// which one a controller takes. Two operators live through the same
    /// generated history of two computers' controllers being plugged in,
    /// chosen for, reset, unplugged and restarted, and of mappings being
    /// deleted. One retires as UXDuck does. The other never does, as before
    /// 2026-11-15. After every step each lookup answers alike on both, for
    /// every controller still plugged in and for one never seen.
    #[test]
    #[allow(clippy::too_many_lines, reason = "one arm for each kind of step")]
    fn retiring_never_changes_what_a_lookup_answers() {
        const NAMES: [&str; 3] = ["Radio", "Pad", "Never chosen for"];
        const VEHICLES: [Option<&str>; 4] = [Some("v1"), Some("v1-before"), Some("v2"), None];
        const KNOWN_BY: [&[&str]; 5] = [&[], &["v1"], &["v2"], &["v1", "v1-before"], &["v1", "v2"]];
        const MAPPINGS: [&str; 3] = ["mode-1", "mode-2", "mapping-own"];
        let mut dropped = 0;
        for history in 0..300_u64 {
            let mut seed = history;
            let mut below = |n: usize| {
                seed = seed
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                usize::try_from(seed >> 33).unwrap_or(0) % n
            };
            let mut retiring = operator_with_no_mappings();
            // What an earlier UXDuck left, and another computer's that this
            // one cannot tell from a controller still plugged in there.
            for n in 0..below(4) {
                retiring.choose_mapping(
                    &format!("gp-session-before-{n}"),
                    NAMES[below(2)],
                    VEHICLES[below(4)],
                    MAPPINGS[below(3)],
                );
            }
            let mut never = retiring.clone();
            // Each computer's controllers: one with a serial number, and
            // those without, by the id of each time they were plugged in.
            let mut plugged: Vec<(usize, String, &str)> = vec![
                (0, "unit-1".into(), "Radio"),
                (1, "keyboard:1".into(), "Keyboard"),
            ];
            let mut made = 0;
            for step in 0..120 {
                let before = retiring.controller_mappings.len();
                match below(20) {
                    0..=3 => {
                        made += 1;
                        let computer = below(2);
                        plugged.push((
                            computer,
                            format!("gp-session-{computer}-{made}"),
                            NAMES[below(2)],
                        ));
                    }
                    // Claimed: it takes theirs, and the window names it.
                    4..=8 => {
                        let (_, id, name) = &plugged[below(plugged.len())];
                        let vehicle = VEHICLES[below(3)];
                        let theirs = never
                            .mapping_for(id, name, vehicle.as_slice())
                            .unwrap_or("mode-2")
                            .to_owned();
                        for operator in [&mut retiring, &mut never] {
                            operator.choose_mapping(id, name, vehicle, &theirs);
                        }
                    }
                    9..=11 => {
                        let (_, id, name) = &plugged[below(plugged.len())];
                        let (vehicle, mapping) = (VEHICLES[below(4)], MAPPINGS[below(3)]);
                        for operator in [&mut retiring, &mut never] {
                            operator.choose_mapping(id, name, vehicle, mapping);
                        }
                    }
                    // Unplugged: only its own computer retires it.
                    12..=15 => {
                        let at = below(plugged.len());
                        if plugged[at].1.starts_with("gp-session-") {
                            let (_, id, _) = plugged.remove(at);
                            retiring.retire_controller(&id);
                            dropped += before - retiring.controller_mappings.len();
                        }
                    }
                    // A computer restarts: every id it gave is gone.
                    16 => {
                        let computer = below(2);
                        let (gone, stay) = plugged.into_iter().partition(|(at, id, _)| {
                            *at == computer && id.starts_with("gp-session-")
                        });
                        plugged = stay;
                        for (_, id, _) in gone {
                            retiring.retire_controller(&id);
                        }
                        dropped += before - retiring.controller_mappings.len();
                    }
                    // Reset, as `controller/forget` does it.
                    17 | 18 => {
                        made += 1;
                        let (_, id, name) = &plugged[below(plugged.len())];
                        for operator in [&mut retiring, &mut never] {
                            operator
                                .controller_mappings
                                .retain(|m| &m.controller_id != id);
                            operator.choose_mapping(id, name, None, &format!("mapping-{made}"));
                        }
                    }
                    // A mapping deleted, as `mapping/remove` does it.
                    _ => {
                        let mapping = MAPPINGS[below(3)];
                        for operator in [&mut retiring, &mut never] {
                            operator
                                .controller_mappings
                                .retain(|m| m.mapping_id != mapping);
                        }
                    }
                }
                let unseen = (2, "gp-session-next".to_owned(), "Radio");
                for (_, id, _) in plugged.iter().chain([&unseen]) {
                    for name in NAMES {
                        for known_by in KNOWN_BY {
                            assert_eq!(
                                retiring.vehicle_mapping(id, name, known_by),
                                never.vehicle_mapping(id, name, known_by),
                                "history {history}, step {step}: {id} as {name} on {known_by:?}"
                            );
                            assert_eq!(
                                retiring.mapping_for(id, name, known_by),
                                never.mapping_for(id, name, known_by),
                                "history {history}, step {step}: {id} as {name} on {known_by:?}"
                            );
                        }
                    }
                }
                // Nothing is retired but what was unplugged, and nothing
                // unmarked ever goes.
                let unmarked = |operator: &Operator| -> Vec<ControllerMapping> {
                    operator
                        .controller_mappings
                        .iter()
                        .filter(|m| !m.retired)
                        .cloned()
                        .collect()
                };
                let live = |m: &ControllerMapping| {
                    plugged.iter().any(|(_, id, _)| *id == m.controller_id)
                        || m.controller_id.starts_with("gp-session-before-")
                };
                assert_eq!(
                    unmarked(&retiring),
                    never
                        .controller_mappings
                        .iter()
                        .filter(|m| live(m))
                        .cloned()
                        .collect::<Vec<_>>(),
                    "history {history}, step {step}"
                );
                // What is retired stays one for each name, vehicle and
                // mapping, however often a controller is plugged in.
                let mut retired: Vec<_> = retiring
                    .controller_mappings
                    .iter()
                    .filter(|m| m.retired)
                    .map(|m| (&m.controller_name, &m.vehicle_id, &m.mapping_id))
                    .collect();
                let all = retired.len();
                retired.sort();
                retired.dedup();
                assert_eq!(retired.len(), all, "history {history}, step {step}");
            }
        }
        assert!(dropped >= 500, "the histories drop little: {dropped}");
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
    fn a_stored_mission_tab_is_dropped_for_the_missions_page() {
        let stored = serde_json::json!({
            "paneOpen": true,
            "dock": {"type": "group", "active": 1, "tabs": [
                {"id": "map", "kind": "map"},
                {"id": "mission", "kind": "mission"},
                {"id": "history", "kind": "history"}
            ]}
        });
        let layout: Layout = serde_json::from_value(stored).unwrap();
        assert!(!layout.dock.contains(TabKind::Mission));
        assert!(layout.dock.contains(TabKind::History));
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
        assert!(
            !layout.dock.contains(TabKind::Mission),
            "the Missions page replaced the Mission tab"
        );
        assert!(!layout.dock.contains(TabKind::Inspector));
        assert!(
            !layout.dock.contains(TabKind::Home),
            "Home stays in the UI drawer"
        );
        assert!(
            !layout.dock.contains(TabKind::Chat),
            "the chat opens on request"
        );
        let kinds: Vec<TabKind> = layout.dock.tabs().iter().map(|tab| tab.kind).collect();
        assert_eq!(
            kinds,
            [TabKind::Map, TabKind::Map3d, TabKind::Control],
            "the two maps above the Cockpit, and nothing else (S267, ADR-0005 2026-11-04)"
        );
    }

    #[test]
    fn a_stored_controllers_tab_goes_or_becomes_the_cockpit() {
        let stored = |dock: DockNode| {
            let mut layout = Layout::initial();
            layout.dock = dock;
            let back: Layout =
                serde_json::from_value(serde_json::to_value(&layout).unwrap()).unwrap();
            back.dock
                .tabs()
                .iter()
                .map(|tab| tab.kind)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            stored(DockNode::of(&[
                TabKind::Map,
                TabKind::Control,
                TabKind::Controllers
            ])),
            [TabKind::Map, TabKind::Control],
            "beside a Cockpit it goes"
        );
        assert_eq!(
            stored(DockNode::of(&[
                TabKind::Map,
                TabKind::Controllers,
                TabKind::Telemetry
            ])),
            [TabKind::Map, TabKind::Control, TabKind::Telemetry],
            "without one it becomes the Cockpit, in its place"
        );
    }

    #[test]
    fn version_13_built_ins_migrate_and_one_whose_default_held_stays() {
        let old_simple = DockNode::Split {
            direction: Direction::Column,
            children: vec![
                DockChild {
                    size: 560,
                    node: DockNode::of(&[TabKind::Map, TabKind::Map3d]),
                },
                DockChild {
                    size: 440,
                    node: DockNode::of(&[
                        TabKind::Control,
                        TabKind::Controllers,
                        TabKind::History,
                        TabKind::Parameters,
                        TabKind::Camera,
                        TabKind::Telemetry,
                    ]),
                },
            ],
        };
        let old_advanced = DockNode::of(&[TabKind::Map, TabKind::Parameters, TabKind::Inspector]);
        let mut layout = Layout::initial();
        layout.dock = old_simple.clone();
        layout.saved[0].dock = old_simple.clone();
        let preset = layout.saved[0].preset.as_mut().unwrap();
        preset.version = 13;
        preset.base = old_simple;
        layout.saved.push(SavedLayout {
            id: "preset-advanced".into(),
            name: "Advanced".into(),
            dock: DockNode::of(&[TabKind::Map, TabKind::Parameters]),
            preset: Some(LayoutPreset {
                kind: LayoutPresetKind::Advanced,
                version: 13,
                base: old_advanced,
            }),
        });
        let back: Layout = serde_json::from_value(serde_json::to_value(&layout).unwrap()).unwrap();
        assert_eq!(back.dock, Layout::simple_dock());
        assert_eq!(back.saved[0].dock, Layout::simple_dock());
        assert_eq!(back.saved[1].dock, Layout::flight_dock());
        assert!(!back.saved[1].dock.contains(TabKind::Parameters));
        for saved in &back.saved {
            let preset = saved.preset.as_ref().unwrap();
            assert_eq!(preset.version, LAYOUT_PRESET_VERSION);
            assert_eq!(preset.base, saved.dock);
        }
        // A built-in stored with this default already keeps its arrangement.
        let mut advanced = Layout::flight_dock();
        if let DockNode::Split { children, .. } = &mut advanced {
            children[0].size = 600;
            children[1].size = 400;
        }
        layout.saved[1].dock = advanced.clone();
        layout.saved[1].preset.as_mut().unwrap().base = Layout::flight_dock();
        layout.current = Some("preset-advanced".into());
        layout.dock = advanced.clone();
        let back: Layout = serde_json::from_value(serde_json::to_value(&layout).unwrap()).unwrap();
        assert_eq!(back.dock, advanced, "the arrangement on screen stays");
        assert_eq!(back.saved[1].dock, advanced);
        assert_eq!(back.saved[0].dock, Layout::simple_dock());
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
    fn both_views_and_every_layout_of_the_persons_own_are_kept() {
        let mut layout = Layout::initial();
        layout.saved.extend((0..100).map(|n| SavedLayout {
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
        assert_eq!(tidy.saved.len(), 102);
        assert_eq!(tidy.saved.iter().filter(|s| s.preset.is_some()).count(), 2);
        assert!(tidy.saved.iter().any(|s| s.id == "custom-99"));
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
        let DockNode::Split { children: rows, .. } = &layout.dock else {
            panic!("the top row over the bottom one");
        };
        let DockNode::Split { children: top, .. } = &rows[0].node else {
            panic!("the map and the 3D map");
        };
        top[0].node.clone()
    }

    /// The Cockpit's group in the flight layout, below the maps.
    fn cockpit_group(layout: &Layout) -> DockNode {
        let DockNode::Split { children: rows, .. } = &layout.dock else {
            panic!("the top row over the bottom one");
        };
        let DockNode::Split {
            children: bottom, ..
        } = &rows[1].node
        else {
            panic!("the bottom row");
        };
        bottom[0].node.clone()
    }

    #[test]
    fn opening_a_tab_shows_the_one_there_or_adds_one_beside_the_map() {
        // None yet: the flight layout opens in the home's place and the tab
        // joins its first group without a map, so the map stays in view.
        let mut layout = Layout {
            dock: DockNode::of(&[TabKind::Home]),
            ..Layout::initial()
        };
        layout.open(TabKind::Vehicles);
        assert!(layout.pane_open);
        assert_eq!(map_group(&layout), DockNode::of(&[TabKind::Map]));
        let added = DockNode::Group {
            tabs: vec![Tab::of(TabKind::Control), Tab::of(TabKind::Vehicles)],
            active: 1,
        };
        assert_eq!(cockpit_group(&layout), added);

        // One there, behind another: it shows, and none is added.
        layout.open(TabKind::Control);
        assert!(matches!(
            cockpit_group(&layout),
            DockNode::Group { active: 0, .. }
        ));
        layout.open(TabKind::Vehicles);
        assert_eq!(cockpit_group(&layout), added);

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

    /// Asked twice for a vehicle's tab, one tab pinned to it; the tab that
    /// follows the selection is left alone (Ben, 2026-11-01: Telemetry
    /// opened twice).
    #[test]
    fn a_vehicles_tab_asked_for_again_is_the_one_pinned_to_it() {
        let mut layout = Layout {
            dock: DockNode::of(&[TabKind::Home]),
            ..Layout::initial()
        };
        layout.open(TabKind::Telemetry);
        let pinned = |vehicle: &str| Tab {
            vehicle_id: Some(vehicle.to_owned()),
            ..Tab::of(TabKind::Telemetry)
        };
        let telemetry = |layout: &Layout| -> Vec<Option<String>> {
            layout
                .dock
                .tabs()
                .into_iter()
                .filter(|t| t.kind == TabKind::Telemetry)
                .map(|t| t.vehicle_id.clone())
                .collect()
        };
        layout.open_tab(pinned("sim-2"));
        layout.open_tab(pinned("sim-2"));
        let shown = telemetry(&layout);
        assert_eq!(shown.len(), 2, "{shown:?}");
        assert!(shown.contains(&None) && shown.contains(&Some("sim-2".to_owned())));
        layout.open_tab(pinned("sim-1"));
        assert_eq!(telemetry(&layout).len(), 3, "another vehicle, another tab");
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
            "Row([chat:chat:,messages:messages:],[map:map:],[inspector:inspector:])"
        );
    }

    /// FB-95: a tab opened, from the palette, a menu or the AI, never goes
    /// in a map's group, so the map stays in view; with no other group it
    /// opens beside the map.
    #[test]
    fn a_new_tab_never_covers_the_map() {
        // Simple: the two maps on top, the Cockpit below; Telemetry joins the Cockpit.
        let mut simple = Layout {
            pane_open: true,
            dock: Layout::simple_dock(),
            ..Layout::initial()
        };
        simple.open(TabKind::Telemetry);
        assert_eq!(
            simple.dock.shape(),
            "Column(Row([map:map:],[map-3d:map-3d:]),[control:control:,telemetry:telemetry:])"
        );

        // The map with the AI on its right: beside the map, half its share.
        let mut beside_chat = Layout {
            pane_open: true,
            dock: DockNode::Split {
                direction: Direction::Row,
                children: vec![
                    DockChild {
                        size: 700,
                        node: DockNode::of(&[TabKind::Map]),
                    },
                    DockChild {
                        size: 300,
                        node: DockNode::of(&[TabKind::Chat]),
                    },
                ],
            },
            ..Layout::initial()
        };
        beside_chat.open(TabKind::Telemetry);
        let DockNode::Split { children, .. } = &beside_chat.dock else {
            panic!("a row");
        };
        assert_eq!(
            beside_chat.dock.shape(),
            "Row([map:map:],[telemetry:telemetry:],[chat:chat:])"
        );
        assert_eq!(
            children.iter().map(|c| c.size).collect::<Vec<_>>(),
            [350, 350, 300]
        );

        // The map above the AI: the map's place split in two.
        let mut above_chat = Layout {
            pane_open: true,
            dock: DockNode::Split {
                direction: Direction::Column,
                children: vec![
                    DockChild {
                        size: 600,
                        node: DockNode::of(&[TabKind::Map]),
                    },
                    DockChild {
                        size: 400,
                        node: DockNode::of(&[TabKind::Chat]),
                    },
                ],
            },
            ..Layout::initial()
        };
        above_chat.open(TabKind::Messages);
        assert_eq!(
            above_chat.dock.shape(),
            "Column(Row([map:map:],[messages:messages:]),[chat:chat:])"
        );

        // A group whose map is hidden shows no map, so it takes the tab.
        let hidden = Tab {
            hidden: true,
            ..Tab::of(TabKind::Map)
        };
        let mut unticked = Layout {
            pane_open: true,
            dock: DockNode::Split {
                direction: Direction::Row,
                children: vec![
                    DockChild {
                        size: 500,
                        node: DockNode::of(&[TabKind::Map3d]),
                    },
                    DockChild {
                        size: 500,
                        node: DockNode::group(vec![hidden, Tab::of(TabKind::Messages)]),
                    },
                ],
            },
            ..Layout::initial()
        };
        unticked.open(TabKind::Telemetry);
        assert_eq!(
            unticked.dock.shape(),
            "Row([map-3d:map-3d:],[map:map:h,messages:messages:,telemetry:telemetry:])"
        );
    }

    #[test]
    fn the_camera_opens_with_the_cockpit() {
        // In the flight layout the camera joins the Cockpit's group below the
        // maps, so the top row stays the two maps.
        let mut layout = Layout {
            pane_open: true,
            dock: Layout::flight_dock(),
            ..Layout::initial()
        };
        layout.open(TabKind::Camera);
        assert!(
            layout
                .dock
                .shape()
                .contains("[control:control:,camera:camera:]")
        );
        assert!(layout.dock.shape().contains("[map-3d:map-3d:]"));
        // Without the Cockpit, as any tab: never in the chat's or the map's
        // group, so beside the map.
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
            "Row([chat:chat:],[map:map:],[camera:camera:])"
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
            calculated_items: Vec::new(),
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
            terms: None,
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
    fn control_actions_are_each_once_and_sorted_however_many() {
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
        // 40 buttons were the most for a type until 2026-11-14.
        let many: Vec<String> = (0..100).map(|i| format!("vehicle:mode:m{i}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        assert_eq!(
            clean_control_actions(&[entry(VehicleClass::Copter, &many)]),
            Some(vec![entry(VehicleClass::Copter, &many)])
        );
    }

    fn operator_with_no_mappings() -> Operator {
        serde_json::from_value(serde_json::json!({
            "id": "op1",
            "name": "Ana",
            "units": "metric",
            "coordinateFormat": "decimal",
            "version": 1
        }))
        .unwrap()
    }

    fn named(name: &str, parameters: &[&str]) -> ParameterGroup {
        ParameterGroup {
            name: name.into(),
            parameters: parameters.iter().map(|&p| p.to_owned()).collect(),
        }
    }

    #[test]
    fn parameter_groups_are_trimmed_and_sorted_however_many() {
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
        ] {
            assert_eq!(clean_parameter_groups(&bad), None, "{bad:?}");
        }
        // 100 groups of 200 parameters were the most until 2026-11-14.
        let parameters: Vec<String> = (0..300).map(|i| format!("PARAM_{i}")).collect();
        let parameters: Vec<&str> = parameters.iter().map(String::as_str).collect();
        let many: Vec<ParameterGroup> = (0..150)
            .map(|i| named(&format!("Group {i:03}"), &parameters))
            .collect();
        assert_eq!(clean_parameter_groups(&many), Some(many));
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
        // 200 pins were the most until 2026-11-14.
        let many: Vec<String> = (0..300).map(|i| format!("received:#{i}")).collect();
        assert_eq!(clean_inspector_pinned(&many), Some(many));
    }
}

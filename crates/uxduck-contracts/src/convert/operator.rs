use uxduck_core::domain::operator as core;
use uxduck_core::events;

use crate::dto::operator as dto;
use crate::dto::state::Actor;

impl From<core::UnitSystem> for dto::UnitSystem {
    fn from(value: core::UnitSystem) -> Self {
        match value {
            core::UnitSystem::Metric => Self::Metric,
            core::UnitSystem::Imperial => Self::Imperial,
        }
    }
}

impl From<dto::UnitSystem> for core::UnitSystem {
    fn from(value: dto::UnitSystem) -> Self {
        match value {
            dto::UnitSystem::Metric => Self::Metric,
            dto::UnitSystem::Imperial => Self::Imperial,
        }
    }
}

impl From<core::CoordinateFormat> for dto::CoordinateFormat {
    fn from(value: core::CoordinateFormat) -> Self {
        match value {
            core::CoordinateFormat::Decimal => Self::Decimal,
            core::CoordinateFormat::Dms => Self::Dms,
            core::CoordinateFormat::Mgrs => Self::Mgrs,
            core::CoordinateFormat::Utm => Self::Utm,
        }
    }
}

impl From<dto::CoordinateFormat> for core::CoordinateFormat {
    fn from(value: dto::CoordinateFormat) -> Self {
        match value {
            dto::CoordinateFormat::Decimal => Self::Decimal,
            dto::CoordinateFormat::Dms => Self::Dms,
            dto::CoordinateFormat::Mgrs => Self::Mgrs,
            dto::CoordinateFormat::Utm => Self::Utm,
        }
    }
}

impl From<core::Basemap> for dto::Basemap {
    fn from(value: core::Basemap) -> Self {
        match value {
            core::Basemap::Imagery => Self::Imagery,
            core::Basemap::Streets => Self::Streets,
        }
    }
}

impl From<dto::Basemap> for core::Basemap {
    fn from(value: dto::Basemap) -> Self {
        match value {
            dto::Basemap::Imagery => Self::Imagery,
            dto::Basemap::Streets => Self::Streets,
        }
    }
}

impl From<&core::Operator> for dto::Operator {
    fn from(value: &core::Operator) -> Self {
        Self {
            id: value.id.clone(),
            name: value.name.clone(),
            units: value.units.into(),
            coordinate_format: value.coordinate_format.into(),
            altitude_frame: value.altitude_frame.into(),
            layout: (&value.layout).into(),
            place_search: value.place_search,
            basemap: value.basemap.into(),
            shortcuts: value
                .shortcuts
                .iter()
                .map(|(action, shortcut)| dto::ShortcutChange {
                    action: action.clone(),
                    shortcut: shortcut.clone(),
                })
                .collect(),
            skipped_confirmations: value.skipped_confirmations.clone(),
            telemetry_layouts: value.telemetry_layouts.iter().map(Into::into).collect(),
            vehicle_telemetry: value
                .vehicle_telemetry
                .iter()
                .map(|(vehicle, layout)| dto::VehicleTelemetryChoice {
                    vehicle_id: vehicle.clone(),
                    layout_id: layout.clone(),
                })
                .collect(),
            parameter_groups: value
                .parameter_groups
                .iter()
                .map(|g| dto::ParameterGroup {
                    name: g.name.clone(),
                    parameters: g.parameters.clone(),
                })
                .collect(),
            inspector_pinned: value.inspector_pinned.clone(),
            control_actions: value.control_actions.iter().map(Into::into).collect(),
            version: value.version,
        }
    }
}

impl From<&dto::ParameterGroup> for core::ParameterGroup {
    fn from(value: &dto::ParameterGroup) -> Self {
        Self {
            name: value.name.clone(),
            parameters: value.parameters.clone(),
        }
    }
}

impl From<&events::Actor> for Actor {
    fn from(value: &events::Actor) -> Self {
        Self {
            client_id: value.client_id.clone(),
            operator_id: value.operator_id.clone(),
        }
    }
}

fn tab_to_dto(kind: core::TabKind) -> dto::TabKind {
    match kind {
        core::TabKind::Chat => dto::TabKind::Chat,
        core::TabKind::Home => dto::TabKind::Home,
        core::TabKind::Map => dto::TabKind::Map,
        core::TabKind::Map3d => dto::TabKind::Map3d,
        core::TabKind::VehicleView => dto::TabKind::VehicleView,
        core::TabKind::Globe => dto::TabKind::Globe,
        core::TabKind::Telemetry | core::TabKind::LegacyVehicle => dto::TabKind::Telemetry,
        core::TabKind::Control => dto::TabKind::Control,
        core::TabKind::Messages => dto::TabKind::Messages,
        core::TabKind::Files => dto::TabKind::Files,
        core::TabKind::Vehicles => dto::TabKind::Vehicles,
        core::TabKind::Parameters => dto::TabKind::Parameters,
        core::TabKind::History => dto::TabKind::History,
        core::TabKind::Camera | core::TabKind::LegacyVideo | core::TabKind::LegacyView3d => {
            dto::TabKind::Camera
        }
        core::TabKind::Mission => dto::TabKind::Mission,
        core::TabKind::Controllers => dto::TabKind::Controllers,
        core::TabKind::Instruments => dto::TabKind::Instruments,
        core::TabKind::FlightDisplay => dto::TabKind::FlightDisplay,
        core::TabKind::GetStarted => dto::TabKind::GetStarted,
        core::TabKind::Inspector => dto::TabKind::Inspector,
        core::TabKind::Settings => dto::TabKind::Settings,
        core::TabKind::Plugin => dto::TabKind::Plugin,
        core::TabKind::Readout => dto::TabKind::Readout,
        core::TabKind::Chart => dto::TabKind::Chart,
        core::TabKind::Practice => dto::TabKind::Practice,
        core::TabKind::Places => dto::TabKind::Places,
        core::TabKind::Flights => dto::TabKind::Flights,
        core::TabKind::Missions => dto::TabKind::Missions,
        core::TabKind::Duck => dto::TabKind::Duck,
    }
}

impl From<dto::TabKind> for core::TabKind {
    fn from(kind: dto::TabKind) -> Self {
        tab_from_dto(kind)
    }
}

fn tab_from_dto(kind: dto::TabKind) -> core::TabKind {
    match kind {
        dto::TabKind::Chat => core::TabKind::Chat,
        dto::TabKind::Home => core::TabKind::Home,
        dto::TabKind::Map => core::TabKind::Map,
        dto::TabKind::Map3d => core::TabKind::Map3d,
        dto::TabKind::VehicleView => core::TabKind::VehicleView,
        dto::TabKind::Globe => core::TabKind::Globe,
        dto::TabKind::Telemetry => core::TabKind::Telemetry,
        dto::TabKind::Control => core::TabKind::Control,
        dto::TabKind::Messages => core::TabKind::Messages,
        dto::TabKind::Files => core::TabKind::Files,
        dto::TabKind::Vehicles => core::TabKind::Vehicles,
        dto::TabKind::Parameters => core::TabKind::Parameters,
        dto::TabKind::History => core::TabKind::History,
        dto::TabKind::Camera => core::TabKind::Camera,
        dto::TabKind::Mission => core::TabKind::Mission,
        dto::TabKind::Controllers => core::TabKind::Controllers,
        dto::TabKind::Instruments => core::TabKind::Instruments,
        dto::TabKind::FlightDisplay => core::TabKind::FlightDisplay,
        dto::TabKind::GetStarted => core::TabKind::GetStarted,
        dto::TabKind::Inspector => core::TabKind::Inspector,
        dto::TabKind::Settings => core::TabKind::Settings,
        dto::TabKind::Plugin => core::TabKind::Plugin,
        dto::TabKind::Readout => core::TabKind::Readout,
        dto::TabKind::Chart => core::TabKind::Chart,
        dto::TabKind::Practice => core::TabKind::Practice,
        dto::TabKind::Places => core::TabKind::Places,
        dto::TabKind::Flights => core::TabKind::Flights,
        dto::TabKind::Missions => core::TabKind::Missions,
        dto::TabKind::Duck => core::TabKind::Duck,
    }
}

fn chart_to_dto(value: &core::ChartSpec) -> dto::ChartSpec {
    dto::ChartSpec {
        series: value
            .series
            .iter()
            .map(|s| dto::ChartSeries {
                item: s.item.clone(),
                colour: s.colour,
                hidden: s.hidden,
            })
            .collect(),
        span_s: value.span_s,
    }
}

fn chart_from_dto(value: dto::ChartSpec) -> core::ChartSpec {
    core::ChartSpec {
        series: value
            .series
            .into_iter()
            .map(|s| core::ChartSeries {
                item: s.item,
                colour: s.colour,
                hidden: s.hidden,
            })
            .collect(),
        span_s: value.span_s,
    }
}

const fn direction_to_dto(value: core::Direction) -> dto::Direction {
    match value {
        core::Direction::Row => dto::Direction::Row,
        core::Direction::Column => dto::Direction::Column,
    }
}

const fn direction_from_dto(value: dto::Direction) -> core::Direction {
    match value {
        dto::Direction::Row => core::Direction::Row,
        dto::Direction::Column => core::Direction::Column,
    }
}

fn dock_to_dto(value: &core::DockNode) -> dto::DockNode {
    match value {
        core::DockNode::Split {
            direction,
            children,
        } => dto::DockNode::Split {
            direction: direction_to_dto(*direction),
            children: children
                .iter()
                .map(|c| dto::DockChild {
                    size: c.size,
                    node: dock_to_dto(&c.node),
                })
                .collect(),
        },
        core::DockNode::Group { tabs, active } => dto::DockNode::Group {
            tabs: tabs
                .iter()
                .map(|t| dto::Tab {
                    id: t.id.clone(),
                    kind: tab_to_dto(t.kind),
                    vehicle_id: t.vehicle_id.clone(),
                    hidden: t.hidden,
                    camera: t.camera.clone(),
                    panel: t.panel.clone(),
                    item: t.item.clone(),
                    camera_behind: t.camera_behind,
                    chart: t.chart.as_ref().map(chart_to_dto),
                })
                .collect(),
            active: *active,
        },
    }
}

fn dock_from_dto(value: dto::DockNode) -> core::DockNode {
    match value {
        dto::DockNode::Split {
            direction,
            children,
        } => core::DockNode::Split {
            direction: direction_from_dto(direction),
            children: children
                .into_iter()
                .map(|c| core::DockChild {
                    size: c.size,
                    node: dock_from_dto(c.node),
                })
                .collect(),
        },
        dto::DockNode::Group { tabs, active } => core::DockNode::Group {
            tabs: tabs
                .into_iter()
                .map(|t| core::Tab {
                    id: t.id,
                    kind: tab_from_dto(t.kind),
                    vehicle_id: t.vehicle_id,
                    hidden: t.hidden,
                    camera: t.camera,
                    panel: t.panel,
                    item: t.item,
                    camera_behind: t.camera_behind,
                    chart: t.chart.map(chart_from_dto),
                })
                .collect(),
            active,
        },
    }
}

impl From<&core::Layout> for dto::Layout {
    fn from(value: &core::Layout) -> Self {
        Self {
            pane_open: value.pane_open,
            dock: dock_to_dto(&value.dock),
            saved: value
                .saved
                .iter()
                .map(|s| dto::SavedLayout {
                    id: s.id.clone(),
                    name: s.name.clone(),
                    dock: dock_to_dto(&s.dock),
                    preset: s.preset.as_ref().map(|p| dto::LayoutPreset {
                        kind: match p.kind {
                            core::LayoutPresetKind::Simple => dto::LayoutPresetKind::Simple,
                            core::LayoutPresetKind::Advanced => dto::LayoutPresetKind::Advanced,
                        },
                        version: p.version,
                        base: dock_to_dto(&p.base),
                    }),
                })
                .collect(),
            arranged: value.arranged,
            flight_opened: value.flight_opened,
            current: value.current.clone(),
        }
    }
}

impl From<dto::Layout> for core::Layout {
    fn from(value: dto::Layout) -> Self {
        Self {
            pane_open: value.pane_open,
            dock: dock_from_dto(value.dock),
            saved: value
                .saved
                .into_iter()
                .map(|s| core::SavedLayout {
                    id: s.id,
                    name: s.name,
                    dock: dock_from_dto(s.dock),
                    preset: s.preset.map(|p| core::LayoutPreset {
                        kind: match p.kind {
                            dto::LayoutPresetKind::Simple => core::LayoutPresetKind::Simple,
                            dto::LayoutPresetKind::Advanced => core::LayoutPresetKind::Advanced,
                        },
                        version: p.version,
                        base: dock_from_dto(p.base),
                    }),
                })
                .collect(),
            arranged: value.arranged,
            flight_opened: value.flight_opened,
            current: value.current,
        }
        .normalised()
    }
}

// The two shapes are the same on the wire: convert through it, so a field
// added on one side cannot be forgotten on the other.
impl From<&uxduck_core::domain::telemetry_layout::TelemetryLayout> for dto::TelemetryLayout {
    fn from(value: &uxduck_core::domain::telemetry_layout::TelemetryLayout) -> Self {
        serde_json::to_value(value)
            .and_then(serde_json::from_value)
            .unwrap_or_else(|_| Self {
                id: value.id.clone(),
                name: value.name.clone(),
                class: None,
                tiles: Vec::new(),
            })
    }
}

impl From<dto::TelemetryLayout> for uxduck_core::domain::telemetry_layout::TelemetryLayout {
    fn from(value: dto::TelemetryLayout) -> Self {
        serde_json::to_value(&value)
            .and_then(serde_json::from_value)
            .unwrap_or_else(|_| Self {
                id: value.id,
                name: value.name,
                class: None,
                tiles: Vec::new(),
            })
    }
}

impl From<uxduck_core::domain::telemetry_layout::VehicleClass> for dto::VehicleClass {
    fn from(value: uxduck_core::domain::telemetry_layout::VehicleClass) -> Self {
        match value {
            uxduck_core::domain::telemetry_layout::VehicleClass::Plane => Self::Plane,
            uxduck_core::domain::telemetry_layout::VehicleClass::Copter => Self::Copter,
            uxduck_core::domain::telemetry_layout::VehicleClass::Rover => Self::Rover,
            uxduck_core::domain::telemetry_layout::VehicleClass::Boat => Self::Boat,
            uxduck_core::domain::telemetry_layout::VehicleClass::Sub => Self::Sub,
            uxduck_core::domain::telemetry_layout::VehicleClass::Other => Self::Other,
        }
    }
}

impl From<dto::VehicleClass> for uxduck_core::domain::telemetry_layout::VehicleClass {
    fn from(value: dto::VehicleClass) -> Self {
        match value {
            dto::VehicleClass::Plane => Self::Plane,
            dto::VehicleClass::Copter => Self::Copter,
            dto::VehicleClass::Rover => Self::Rover,
            dto::VehicleClass::Boat => Self::Boat,
            dto::VehicleClass::Sub => Self::Sub,
            dto::VehicleClass::Other => Self::Other,
        }
    }
}

impl From<&core::ControlActions> for dto::ControlActions {
    fn from(value: &core::ControlActions) -> Self {
        Self {
            class: value.class.into(),
            actions: value.actions.clone(),
        }
    }
}

impl From<&dto::ControlActions> for core::ControlActions {
    fn from(value: &dto::ControlActions) -> Self {
        Self {
            class: value.class.into(),
            actions: value.actions.clone(),
        }
    }
}

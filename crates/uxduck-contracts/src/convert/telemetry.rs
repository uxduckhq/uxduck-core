use uxduck_core::domain::telemetry as core;

use crate::dto::telemetry as dto;

impl From<core::AltitudeFrame> for dto::AltitudeFrame {
    fn from(value: core::AltitudeFrame) -> Self {
        match value {
            core::AltitudeFrame::Amsl => Self::Amsl,
            core::AltitudeFrame::Ellipsoid => Self::Ellipsoid,
            core::AltitudeFrame::AboveHome => Self::AboveHome,
            core::AltitudeFrame::AboveGround => Self::AboveGround,
        }
    }
}

impl From<dto::AltitudeFrame> for core::AltitudeFrame {
    fn from(value: dto::AltitudeFrame) -> Self {
        match value {
            dto::AltitudeFrame::Amsl => Self::Amsl,
            dto::AltitudeFrame::Ellipsoid => Self::Ellipsoid,
            dto::AltitudeFrame::AboveHome => Self::AboveHome,
            dto::AltitudeFrame::AboveGround => Self::AboveGround,
        }
    }
}

impl From<core::Unit> for dto::Unit {
    fn from(value: core::Unit) -> Self {
        match value {
            core::Unit::Meter => Self::Meter,
            core::Unit::MeterPerSecond => Self::MeterPerSecond,
            core::Unit::Degree => Self::Degree,
            core::Unit::DegreePerSecond => Self::DegreePerSecond,
            core::Unit::Volt => Self::Volt,
            core::Unit::Ampere => Self::Ampere,
            core::Unit::Percent => Self::Percent,
            core::Unit::MeterPerSecondSquared => Self::MeterPerSecondSquared,
            core::Unit::MilliampHour => Self::MilliampHour,
            core::Unit::DegreeCelsius => Self::DegreeCelsius,
            core::Unit::Second => Self::Second,
            core::Unit::Time => Self::Time,
            core::Unit::Count => Self::Count,
            core::Unit::None => Self::None,
        }
    }
}

impl From<dto::Unit> for core::Unit {
    fn from(value: dto::Unit) -> Self {
        match value {
            dto::Unit::Meter => Self::Meter,
            dto::Unit::MeterPerSecond => Self::MeterPerSecond,
            dto::Unit::Degree => Self::Degree,
            dto::Unit::DegreePerSecond => Self::DegreePerSecond,
            dto::Unit::Volt => Self::Volt,
            dto::Unit::Ampere => Self::Ampere,
            dto::Unit::Percent => Self::Percent,
            dto::Unit::MeterPerSecondSquared => Self::MeterPerSecondSquared,
            dto::Unit::MilliampHour => Self::MilliampHour,
            dto::Unit::DegreeCelsius => Self::DegreeCelsius,
            dto::Unit::Second => Self::Second,
            dto::Unit::Time => Self::Time,
            dto::Unit::Count => Self::Count,
            dto::Unit::None => Self::None,
        }
    }
}

impl From<&core::PositionEstimate> for dto::PositionEstimate {
    fn from(p: &core::PositionEstimate) -> Self {
        Self {
            source: p.source.clone(),
            component_id: p.component_id,
            latitude_deg: p.latitude_deg,
            longitude_deg: p.longitude_deg,
            altitudes: p
                .altitudes
                .iter()
                .map(|a| dto::Altitude {
                    meters: a.meters,
                    frame: a.frame.into(),
                })
                .collect(),
            heading_deg: p.heading_deg,
            course_deg: p.course_deg,
            ground_speed_mps: p.ground_speed_mps,
            climb_mps: p.climb_mps,
            horizontal_accuracy_m: p.horizontal_accuracy_m,
            vertical_accuracy_m: p.vertical_accuracy_m,
            time_us: p.time_us,
        }
    }
}

impl From<&core::TelemetryItem> for dto::TelemetryItem {
    fn from(i: &core::TelemetryItem) -> Self {
        Self {
            key: i.key.clone(),
            label: i.label.clone(),
            unit: i.unit.into(),
            value: i.value,
            text: i.text.clone(),
            rate_per_s: None,
            source: i.source.clone(),
            component_id: i.component_id,
            time_us: i.time_us,
        }
    }
}

impl From<&core::VehicleTelemetry> for dto::VehicleTelemetry {
    fn from(t: &core::VehicleTelemetry) -> Self {
        Self {
            vehicle_id: t.vehicle_id.clone(),
            positions: t.positions.iter().map(Into::into).collect(),
            items: t.items.iter().map(Into::into).collect(),
        }
    }
}

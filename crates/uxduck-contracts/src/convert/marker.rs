use uxduck_core::domain::marker as m;

use crate::dto::marker as dto;

impl From<m::MarkerLook> for dto::MarkerLook {
    fn from(v: m::MarkerLook) -> Self {
        match v {
            m::MarkerLook::Person => Self::Person,
            m::MarkerLook::GroundStation => Self::GroundStation,
            m::MarkerLook::Boat => Self::Boat,
            m::MarkerLook::Car => Self::Car,
            m::MarkerLook::Flag => Self::Flag,
            m::MarkerLook::Pin => Self::Pin,
            m::MarkerLook::Building => Self::Building,
        }
    }
}

impl From<dto::MarkerLook> for m::MarkerLook {
    fn from(v: dto::MarkerLook) -> Self {
        match v {
            dto::MarkerLook::Person => Self::Person,
            dto::MarkerLook::GroundStation => Self::GroundStation,
            dto::MarkerLook::Boat => Self::Boat,
            dto::MarkerLook::Car => Self::Car,
            dto::MarkerLook::Flag => Self::Flag,
            dto::MarkerLook::Pin => Self::Pin,
            dto::MarkerLook::Building => Self::Building,
        }
    }
}

impl From<m::MarkerSource> for dto::MarkerSource {
    fn from(v: m::MarkerSource) -> Self {
        match v {
            m::MarkerSource::Manual => Self::Manual,
            m::MarkerSource::Computer => Self::Computer,
            m::MarkerSource::Serial { port, baud } => Self::Serial { port, baud },
            m::MarkerSource::Udp { port } => Self::Udp { port },
            m::MarkerSource::Device => Self::Device,
        }
    }
}

impl From<dto::MarkerSource> for m::MarkerSource {
    fn from(v: dto::MarkerSource) -> Self {
        match v {
            dto::MarkerSource::Manual => Self::Manual,
            dto::MarkerSource::Computer => Self::Computer,
            dto::MarkerSource::Serial { port, baud } => Self::Serial { port, baud },
            dto::MarkerSource::Udp { port } => Self::Udp { port },
            dto::MarkerSource::Device => Self::Device,
        }
    }
}

impl From<m::MarkerOutline> for dto::MarkerOutline {
    fn from(v: m::MarkerOutline) -> Self {
        match v {
            m::MarkerOutline::Rectangle { width_m, depth_m } => {
                Self::Rectangle { width_m, depth_m }
            }
            m::MarkerOutline::Corners { corners_m } => Self::Corners { corners_m },
            m::MarkerOutline::Round { radius_m } => Self::Round { radius_m },
        }
    }
}

impl From<dto::MarkerOutline> for m::MarkerOutline {
    fn from(v: dto::MarkerOutline) -> Self {
        match v {
            dto::MarkerOutline::Rectangle { width_m, depth_m } => {
                Self::Rectangle { width_m, depth_m }
            }
            dto::MarkerOutline::Corners { corners_m } => Self::Corners { corners_m },
            dto::MarkerOutline::Round { radius_m } => Self::Round { radius_m },
        }
    }
}

impl From<m::MarkerShape> for dto::MarkerShape {
    fn from(v: m::MarkerShape) -> Self {
        let out = |points: Vec<uxduck_core::domain::plan::LatLon>| {
            points
                .into_iter()
                .map(|p| crate::dto::plan::LatLon {
                    latitude_deg: p.latitude_deg,
                    longitude_deg: p.longitude_deg,
                })
                .collect()
        };
        match v {
            m::MarkerShape::Line { points } => Self::Line {
                points: out(points),
            },
            m::MarkerShape::Area { points } => Self::Area {
                points: out(points),
            },
            m::MarkerShape::Circle { radius_m } => Self::Circle { radius_m },
        }
    }
}

impl From<dto::MarkerShape> for m::MarkerShape {
    fn from(v: dto::MarkerShape) -> Self {
        let into = |points: Vec<crate::dto::plan::LatLon>| points.iter().map(Into::into).collect();
        match v {
            dto::MarkerShape::Line { points } => Self::Line {
                points: into(points),
            },
            dto::MarkerShape::Area { points } => Self::Area {
                points: into(points),
            },
            dto::MarkerShape::Circle { radius_m } => Self::Circle { radius_m },
        }
    }
}

impl From<m::MarkerColour> for dto::MarkerColour {
    fn from(v: m::MarkerColour) -> Self {
        match v {
            m::MarkerColour::Blue => Self::Blue,
            m::MarkerColour::Red => Self::Red,
            m::MarkerColour::Orange => Self::Orange,
            m::MarkerColour::Yellow => Self::Yellow,
            m::MarkerColour::Green => Self::Green,
            m::MarkerColour::Teal => Self::Teal,
            m::MarkerColour::Purple => Self::Purple,
            m::MarkerColour::Pink => Self::Pink,
            m::MarkerColour::Grey => Self::Grey,
            m::MarkerColour::Black => Self::Black,
        }
    }
}

impl From<dto::MarkerColour> for m::MarkerColour {
    fn from(v: dto::MarkerColour) -> Self {
        match v {
            dto::MarkerColour::Blue => Self::Blue,
            dto::MarkerColour::Red => Self::Red,
            dto::MarkerColour::Orange => Self::Orange,
            dto::MarkerColour::Yellow => Self::Yellow,
            dto::MarkerColour::Green => Self::Green,
            dto::MarkerColour::Teal => Self::Teal,
            dto::MarkerColour::Purple => Self::Purple,
            dto::MarkerColour::Pink => Self::Pink,
            dto::MarkerColour::Grey => Self::Grey,
            dto::MarkerColour::Black => Self::Black,
        }
    }
}

impl From<&m::Marker> for dto::Marker {
    fn from(v: &m::Marker) -> Self {
        Self {
            id: v.id.clone(),
            name: v.name.clone(),
            look: v.look.into(),
            latitude_deg: v.latitude_deg,
            longitude_deg: v.longitude_deg,
            heading_deg: v.heading_deg,
            source: v.source.clone().into(),
            watch_links: v.watch_links,
            outline: v.outline.clone().map(Into::into),
            height_m: v.height_m,
            shape: v.shape.clone().map(Into::into),
            colour: v.colour.map(Into::into),
            dashed: v.dashed,
            outline_only: v.outline_only,
            live: None,
            created_at_us: v.created_at_us,
            version: v.version,
        }
    }
}

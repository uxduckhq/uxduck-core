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
            live: None,
            created_at_us: v.created_at_us,
            version: v.version,
        }
    }
}

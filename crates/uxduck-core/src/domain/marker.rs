//! Markers (ADR-0035): named things on the map besides vehicles, such as a
//! launch site, a ground station or a boat. Everyone on this computer sees
//! the same ones.

use serde::{Deserialize, Serialize};

/// How a marker is drawn (M5): UXDuck's own icons and models only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MarkerLook {
    Person,
    GroundStation,
    Boat,
    Car,
    Flag,
    #[default]
    Pin,
}

/// Where its position comes from (M2).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum MarkerSource {
    #[default]
    Manual,
    Computer,
    Serial {
        port: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        baud: Option<u32>,
    },
    Udp {
        port: u16,
    },
    Device,
}

impl MarkerSource {
    /// Read by the daemon's own reader: a GPS receiver, not the computer.
    #[must_use]
    pub const fn read_by_host(&self) -> bool {
        matches!(self, Self::Serial { .. } | Self::Udp { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Marker {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub look: MarkerLook,
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    /// Where it faces, clockwise from true north; `None` draws no heading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading_deg: Option<f64>,
    /// Live from a source, or by hand; the position above is the last kept.
    #[serde(default, skip_serializing_if = "is_manual")]
    pub source: MarkerSource,
    /// Shown to watch links (M9), unless the person turned it off.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub watch_links: bool,
    pub created_at_us: u64,
    pub version: u64,
}

fn is_manual(source: &MarkerSource) -> bool {
    *source == MarkerSource::Manual
}

const fn yes() -> bool {
    true
}

#[allow(clippy::trivially_copy_pass_by_ref, reason = "serde's signature")]
const fn is_true(value: &bool) -> bool {
    *value
}

//! Markers (ADR-0035): named things on the map besides vehicles, such as a
//! launch site, a ground station or a boat. Everyone on this computer sees
//! the same ones.

use serde::{Deserialize, Serialize};

use crate::domain::plan::LatLon;

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
    /// A building, a mast or a tree (ADR-0052 B1): drawn as its outline
    /// and height rather than an icon.
    Building,
}

/// A building's outline (ADR-0052 B2), in metres about the marker's point,
/// as if it faced north: the marker's heading turns it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum MarkerOutline {
    /// `width_m` across its front (east–west facing north), `depth_m` along
    /// its heading.
    Rectangle {
        width_m: f64,
        depth_m: f64,
    },
    /// East and north of the point, in order round the outline.
    Corners {
        corners_m: Vec<[f64; 2]>,
    },
    Round {
        radius_m: f64,
    },
}

/// A line, an area or a circle drawn on the map (ADR-0053 D1), in
/// latitude and longitude (D2). The marker's point is the shape's middle,
/// so moving the point moves the shape; a circle is about the point.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum MarkerShape {
    Line { points: Vec<LatLon> },
    Area { points: Vec<LatLon> },
    Circle { radius_m: f64 },
}

/// A marker's colour from the fixed palette (ADR-0053 D3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MarkerColour {
    Blue,
    Red,
    Orange,
    Yellow,
    Green,
    Teal,
    Purple,
    Pink,
    Grey,
    Black,
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
    /// A building's outline (ADR-0052 B2); only a building has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outline: Option<MarkerOutline>,
    /// A building's height above the highest ground under it (B3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height_m: Option<f64>,
    /// A line, an area or a circle (ADR-0053); a point without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<MarkerShape>,
    /// Without one, a point is the usual dark disc and a shape is blue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub colour: Option<MarkerColour>,
    /// A line or an outline drawn dashed (D4).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dashed: bool,
    /// An area or a circle drawn without its faint fill (D4).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub outline_only: bool,
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

//! Markers on the map (ADR-0035).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How a marker is drawn: UXDuck's own icons and models.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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

/// Where a marker's position comes from (ADR-0035 M2): set by hand, or live
/// from a source until it is set by hand again.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum MarkerSource {
    /// Where the person put it; it stays there.
    #[default]
    Manual,
    /// This computer's location (ADR-0030), while a person has turned it on.
    Computer,
    /// A GPS receiver on a serial port, such as a USB puck, speaking NMEA
    /// 0183.
    Serial {
        /// The port, as `serial/list` lists it.
        port: String,
        /// Its speed; absent, UXDuck tries the usual ones.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        baud: Option<u32>,
    },
    /// NMEA 0183 sentences sent to this computer over UDP, as chartplotters
    /// and NMEA 2000 gateways send them (often to port 10110).
    Udp { port: u16 },
    /// A window shares its device's position with `marker/share`: a phone,
    /// or this computer's browser.
    Device,
}

/// How a live marker's source stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum MarkerLiveState {
    /// Open, and no position yet.
    Waiting,
    /// Positions arrive; the latest is `fix`.
    Live,
    /// The source cannot be read; `reason` says why. A fix from before is
    /// kept, as last seen.
    Failed,
}

/// A live marker's latest from its source: in memory only (ADR-0035 M11),
/// never journaled, sent as `marker/live`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MarkerLive {
    pub state: MarkerLiveState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<MarkerFix>,
    /// Why it is waiting or failed, in the words the app shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The device sharing it, such as "iPhone", for a `device` source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

/// `marker/share`: this window's device position for a marker whose
/// position comes from a device. One device shares a marker at a time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MarkerShareParams {
    pub id: String,
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    /// From the device's compass, clockwise from true north.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading_deg: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accuracy_m: Option<f64>,
    /// What the device is called in the app, such as "iPhone" or "Mac".
    pub device: String,
}

/// One position from a marker's source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MarkerFix {
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    /// Where it faces, from the source's heading or, moving, its course.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading_deg: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accuracy_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub satellites: Option<u32>,
    /// When UXDuck received it, Unix milliseconds.
    #[schemars(schema_with = "crate::safe::integer")]
    pub at_ms: u64,
}

/// `marker/live`: one live marker's latest, to windows that may see where
/// people are (the `location` capability).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MarkerLiveChanged {
    pub id: String,
    /// Absent once it is set by hand again or removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live: Option<MarkerLive>,
}

/// A named thing on the map besides a vehicle, the same in every window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Marker {
    pub id: String,
    pub name: String,
    pub look: MarkerLook,
    /// Set by hand, or the last position kept for it.
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    /// Where it faces, degrees clockwise from true north; absent when it has
    /// no heading. A live marker faces as its source says, this when it
    /// says nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading_deg: Option<f64>,
    #[serde(default, skip_serializing_if = "is_manual")]
    pub source: MarkerSource,
    /// Shown to watch links (M9); on unless turned off.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub watch_links: bool,
    /// A live marker's latest, for those who may see where people are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live: Option<MarkerLive>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub created_at_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

const fn yes() -> bool {
    true
}

#[allow(clippy::trivially_copy_pass_by_ref, reason = "serde's signature")]
const fn is_true(value: &bool) -> bool {
    *value
}

#[allow(clippy::trivially_copy_pass_by_ref, reason = "serde's signature")]
fn is_manual(source: &MarkerSource) -> bool {
    *source == MarkerSource::Manual
}

/// Every marker on this computer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MarkerListResult {
    pub markers: Vec<Marker>,
}

/// Adds a marker at a latitude and longitude. Without a name it is named
/// after its look ("Flag", "Boat").
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MarkerAddParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub look: Option<MarkerLook>,
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    /// Degrees clockwise from true north, 0 to 360.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading_deg: Option<f64>,
    /// Live from this computer or a GPS receiver; by hand when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<MarkerSource>,
}

/// Changes a marker edited from `expectedVersion`: only the fields given.
/// `headingDeg: null` takes its heading away.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MarkerUpdateParams {
    pub id: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub expected_version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub look: Option<MarkerLook>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latitude_deg: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub longitude_deg: Option<f64>,
    #[serde(
        default,
        deserialize_with = "crate::nullable::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub heading_deg: Option<Option<f64>>,
    /// Another source; `manual` keeps it where it was last seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<MarkerSource>,
    /// Shown to watch links, or not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch_links: Option<bool>,
}

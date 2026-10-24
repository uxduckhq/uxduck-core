//! Vehicles on the wire (ADR-0004).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Presence {
    /// Heartbeats arriving on time.
    Online,
    /// Overdue by a short margin: probably a dropout.
    Stale,
    /// Silent past the long threshold.
    Offline,
}

/// A vehicle: its id, where it is reached, what it is and whether it is heard.
#[allow(
    clippy::struct_excessive_bools,
    reason = "what the heartbeat says, and the user's choices about it"
)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Vehicle {
    /// The VID: a UUID v5 of the autopilot's UID once known.
    pub id: String,
    pub name: String,
    /// Earlier ids of the same vehicle, such as the provisional one.
    pub aliases: Vec<String>,
    pub link_id: String,
    /// The source address it is heard from on its link.
    pub peer: String,
    pub system_id: u8,
    pub component_ids: Vec<u8>,
    /// `MAV_AUTOPILOT`, such as `Ardupilotmega` or `Px4`.
    pub autopilot: String,
    /// `MAV_TYPE`, such as `Quadrotor` or `FixedWing`.
    pub vehicle_type: String,
    /// The autopilot's hardware id, from `AUTOPILOT_VERSION`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
    /// The first 8 characters of the git hash it was built from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware_commit: Option<String>,
    pub presence: Presence,
    /// Latest received heartbeat on direct vehicle reads; state snapshots
    /// keep the timestamp of the last persisted vehicle change.
    #[schemars(schema_with = "crate::safe::integer")]
    pub last_heartbeat_us: u64,
    /// Age at the time of a direct vehicle read. Absent on persisted state
    /// snapshots and hosts that cannot observe individual heartbeats.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub heartbeat_age_ms: Option<u64>,
    /// From the heartbeat.
    pub armed: bool,
    /// The flight mode by ArduPilot's name, such as `LOITER`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    pub saved: bool,
    pub dismissed: bool,
    /// Kept out of the account's library although sync is on (S16).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub local_only: bool,
    /// The person's labels on it, such as "Survey" (S106): any number,
    /// meaning nothing to UXDuck. Absent when it has none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Web pages kept with it, such as its radio's settings page, set
    /// with `vehicle/bookmarks`. Absent when it has none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bookmarks: Vec<VehicleBookmark>,
    /// Its length, or wingspan, in centimetres, set with `vehicle/size` for
    /// the 3D map. Absent, the map draws it the size of the one its
    /// autopilot's simulator flies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_cm: Option<u32>,
    /// The traffic its ADS-B receiver reports is used, set with
    /// `vehicle/traffic` (S168). Absent when it is not.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub traffic: bool,
    /// The model it is drawn as, set with `vehicle/look`. Absent, the one
    /// its type gives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub look: Option<VehicleLook>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleListResult {
    pub vehicles: Vec<Vehicle>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleSaveParams {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Keep it out of the account's library, or put it back (S16).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_only: Option<bool>,
}

/// A model the person chose to draw a vehicle with (ADR-0035 M5), in place
/// of the one its type gives: UXDuck's own models only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum VehicleLook {
    Plane,
    Vtol,
    Multicopter,
    Rover,
    Boat,
    Submarine,
    Airship,
    Jet,
}

/// `vehicle/look`: the model a vehicle is drawn as on both maps, such as a
/// copter drawn as a boat it rides on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleLookParams {
    /// The vehicle, by id or an earlier alias.
    pub id: String,
    /// Absent gives it back the model its type gives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub look: Option<VehicleLook>,
}

/// `vehicle/size`: a vehicle's length, or wingspan, for the 3D map.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleSizeParams {
    /// The vehicle, by id or an earlier alias.
    pub id: String,
    /// Centimetres, 1 to 100000 (1 km). Absent gives it back to the size
    /// of the one its autopilot's simulator flies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_cm: Option<u32>,
}

/// `vehicle/label`: labels put on and taken off several vehicles at once
/// (S106). Taking one off every vehicle that carries it removes it; taking
/// it off and putting another on renames it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleLabelParams {
    /// The vehicles, by id or an earlier alias.
    pub ids: Vec<String>,
    /// Labels to put on each; one already in use, in any case, keeps the
    /// spelling it has.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add: Vec<String>,
    /// Labels to take off each, in any case.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

/// A web page kept with a vehicle, such as the settings page its radio
/// serves on the local network.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleBookmark {
    /// 1 to 40 characters, such as "Radio".
    pub name: String,
    /// An `http` or `https` address, such as `http://192.168.1.20`.
    pub url: String,
}

/// `vehicle/bookmarks`: a vehicle's web pages, all of them, in order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleBookmarksParams {
    /// The vehicle, by id or an earlier alias.
    pub id: String,
    /// Any number; an empty list removes them all.
    pub bookmarks: Vec<VehicleBookmark>,
}

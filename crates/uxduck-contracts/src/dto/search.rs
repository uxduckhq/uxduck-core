//! Search from the input (ADR-0009): one daemon method over entities, which
//! the UI, the agent and the MCP server share.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What a hit is; the UI shows it on the right.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SearchKind {
    Place,
    Vehicle,
    Chat,
    Folder,
    Parameter,
    Mission,
    Telemetry,
    Setting,
    /// A note in a vehicle's history (S69).
    Note,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchQueryParams {
    pub query: String,
    /// Only these kinds; all when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<SearchKind>>,
    /// At most this many hits (default 20, at most 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Coordinates {
    pub latitude_deg: f64,
    pub longitude_deg: f64,
}

/// One hit: what it is, and where it lives.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub kind: SearchKind,
    /// The entity's id: a thread, folder, vehicle, parameter (`<vehicle
    /// id>/<NAME>`), mission (its vehicle's id), telemetry item
    /// (`<vehicle id>|<key>`), setting (a section of Settings) or place.
    pub id: String,
    pub title: String,
    /// Where it lives, such as "Copter 1" for a parameter.
    pub subtitle: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place: Option<Coordinates>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
}

/// A saved place.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SavedPlace {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_id: Option<String>,
    pub name: String,
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub created_at_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

/// Saves a place on the operator's list (the connection's operator).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlaceSaveParams {
    pub name: String,
    pub latitude_deg: f64,
    pub longitude_deg: f64,
}

/// Renames a saved place without changing its identity or coordinates.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlaceUpdateParams {
    pub id: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub expected_version: u64,
    pub name: String,
}

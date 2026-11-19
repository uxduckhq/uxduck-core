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

/// Global place-name search through UXDuck's hosted geocoder. Suggestions
/// have names and keys; `place/locate` resolves the one chosen to coordinates.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlaceSearchParams {
    /// A town, landmark or address, from 2 to 200 characters.
    pub query: String,
    /// Default 5, at most 10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Optional proximity hint, rounded to a neighbourhood; never restricts
    /// the search area or requests this computer's location.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub near: Option<Coordinates>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlaceSuggestion {
    pub name: String,
    pub detail: String,
    pub key: String,
    pub text: String,
    /// Pass this same hint to `place/locate` with the key and text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub near: Option<Coordinates>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlaceSearchResult {
    pub suggestions: Vec<PlaceSuggestion>,
    pub attribution: String,
}

/// The chosen suggestion's key, full text and original proximity hint.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlaceLocateParams {
    pub key: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub near: Option<Coordinates>,
}

/// Coordinates for `terrain/at`, `sim/start` or saving a place. No simulation
/// is started and no place is saved by this lookup.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LocatedPlace {
    pub name: String,
    pub detail: String,
    pub latitude_deg: f64,
    pub longitude_deg: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlaceLocateResult {
    pub place: LocatedPlace,
    pub attribution: String,
}

/// A named area to outline for a survey (FB-177): a park, a field, a
/// suburb. `near`, rounded to a neighbourhood like `place/search`'s, picks
/// between places of the same name.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlaceAreaParams {
    /// The area's name as the person said it, then its town or district,
    /// such as "Great Lawn, Manhattan" or "Central Park, New York", not a
    /// street address: 2 to 200 characters.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub near: Option<Coordinates>,
    /// The chat asking; the agent's turns fill it in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

/// An outline found for a named area, from OpenStreetMap: waiting on the
/// map for a person to confirm it, or confirmed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlaceArea {
    pub id: String,
    pub name: String,
    /// Where it is, for people: "Central Park, Manhattan, New York, United States".
    pub detail: String,
    /// What OpenStreetMap calls it, such as "park" or "suburb".
    pub kind: String,
    /// 3 or more corners in order round it, as a survey's `area` takes them.
    pub points: Vec<crate::dto::plan::LatLon>,
    /// What the outline leaves out of OpenStreetMap's, in words: other parts
    /// of the place, or holes in it such as a pond.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub attribution: String,
    /// The chat that asked, whose operator's windows show it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_id: Option<String>,
}

/// The outline a person confirmed on the map.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlaceAreaResult {
    pub area: ConfirmedPlaceArea,
}

/// An outline the person confirmed, for planning by its `id` (FB-177): a
/// plan takes it from `plan/create`'s `placeAreas`, so it uses exactly the
/// confirmed corners, which are never carried by the answer itself.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmedPlaceArea {
    pub id: String,
    pub name: String,
    pub detail: String,
    pub kind: String,
    /// How many corners the confirmed outline has.
    pub corners: u32,
    pub south_deg: f64,
    pub west_deg: f64,
    pub north_deg: f64,
    pub east_deg: f64,
    /// Some of its corners, evenly spaced round it, only to see its shape.
    /// A plan never takes these.
    pub preview: Vec<crate::dto::plan::LatLon>,
    /// How to plan from it, and how much of it `preview` shows, in words.
    pub how_to_plan: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub attribution: String,
}

/// The outlines waiting for a person on the map.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlaceAreasResult {
    pub areas: Vec<PlaceArea>,
}

/// A person's word on an outline waiting on the map: it is the area, or it is not.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlaceAreaAnswerParams {
    pub id: String,
    pub confirmed: bool,
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

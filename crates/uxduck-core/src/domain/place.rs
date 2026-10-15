//! Saved places (ADR-0009 search in the input): an operator's own list of
//! named coordinates, found by search with no network.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedPlace {
    pub id: String,
    /// Whose list it is on; `None` for everyone on this machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_id: Option<String>,
    pub name: String,
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    pub created_at_us: u64,
    pub version: u64,
}

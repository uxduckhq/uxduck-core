//! A fork's firmware profile (ADR-0044 F12, S254): one file its CI
//! publishes, naming its parameter descriptions and its MAVLink dialects.
//! Added once by its URL; the parameter set and dialect it brings are its
//! own, bound to its commit, and fetched again daily and on a refresh.

use serde::{Deserialize, Serialize};

use crate::domain::param_definitions::Firmware;

/// A profile a person added, as last read from its URL.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FirmwareProfile {
    pub id: String,
    /// Where the fork's CI publishes it: `https://`, or a server on this
    /// computer.
    pub url: String,
    /// Its name: the one the person gave it, else the profile's own
    /// label, and its host until it is first read.
    pub label: String,
    /// The name the person gave it, kept over the profile's own label on
    /// every read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// What the profile says, once read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<Firmware>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The commit its vehicles are built from; with none, its parameters
    /// describe every vehicle of its firmware and its dialects may only add
    /// messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The person's set holding its parameter descriptions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own_set_id: Option<String>,
    /// The dialect read from its MAVLink files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialect_id: Option<String>,
    pub added_at_us: u64,
    /// When it was last read from its URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at_us: Option<u64>,
    /// Its version for sync.
    #[serde(default)]
    pub updated_at_us: u64,
}

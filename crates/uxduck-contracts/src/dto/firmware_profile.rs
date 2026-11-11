//! A fork's firmware profile on the wire (ADR-0044 F12, S254): the file a
//! fork's CI publishes naming its parameter descriptions and MAVLink
//! dialects, added once by its URL.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::param_definitions::Firmware;

/// A firmware profile a person added.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FirmwareProfile {
    pub id: String,
    pub url: String,
    /// The name the person gave it, else the profile's label; its host
    /// until it is first read.
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<Firmware>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The commit its vehicles are built from, which both its parameters
    /// and its dialects describe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The parameter set it brought (`param/definitions/list`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own_set_id: Option<String>,
    /// The dialect it brought (`mavlink/dialects/list`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialect_id: Option<String>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub added_at_us: u64,
    /// When it was last read from its URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub fetched_at_us: Option<u64>,
    /// Why the last refresh failed; what it brought before stays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// `firmware/add`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FirmwareAddParams {
    /// Where the fork's CI publishes the profile: `https://`, or a server
    /// on this computer.
    pub url: String,
    /// What to call it, kept over the label the profile gives itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `firmware/rename`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FirmwareRenameParams {
    pub id: String,
    /// Its new name, kept over the label the profile gives itself, and
    /// given to the parameters and dialect it brought.
    pub name: String,
}

/// `firmware/list`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FirmwareListParams {
    /// Fetch every profile again first.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub refresh: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FirmwareListResult {
    /// Newest first.
    pub profiles: Vec<FirmwareProfile>,
}

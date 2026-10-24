//! Parameter definitions on the wire (ADR-0021 P1): the sets, and the
//! catalogue that publishes ArduPilot's (P2). A set's file in the catalogue
//! is a gzipped [`DefinitionSet`] in this same JSON.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::sim::CatalogueFile;

/// The ArduPilot firmware a set documents. A helicopter runs `ArduCopter`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum Firmware {
    ArduCopter,
    ArduPlane,
    Rover,
    ArduSub,
    AntennaTracker,
    Blimp,
}

/// Where a set came from: ArduPilot's own, from the catalogue, or added by a
/// person for their fork.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case", tag = "type")]
pub enum DefinitionSource {
    Ardupilot,
    #[serde(rename_all = "camelCase")]
    Own {
        label: String,
    },
}

/// ArduPilot's `@User`: what most people need to see, or not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum UserLevel {
    Standard,
    #[default]
    Advanced,
}

/// Advice, not a limit: a value outside it can be written after a
/// confirmation (P7).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ParamRange {
    pub low: f64,
    pub high: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ValueName {
    pub value: f64,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BitName {
    pub bit: u8,
    pub name: String,
}

/// What one parameter means, for one firmware at one version.
#[expect(clippy::struct_excessive_bools, reason = "ArduPilot's own flags")]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamDefinition {
    pub name: String,
    pub display_name: String,
    pub description: String,
    /// The prefix it is documented under, such as `BATT_`.
    pub group: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<ParamRange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub increment: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<ValueName>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bitmask: Vec<BitName>,
    #[serde(default)]
    pub user: UserLevel,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reboot_required: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub volatile: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub calibration: bool,
    /// The library that documents this variant, such as
    /// `AC_AttitudeControl_Heli`, when the name is documented more than once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

/// Every parameter of one firmware build.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DefinitionSet {
    pub source: DefinitionSource,
    pub firmware: Firmware,
    /// `4.5.7`, or `latest` for the development build's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    pub parameters: Vec<ParamDefinition>,
}

/// The definitions catalogue's manifest (P2), signed with the release key as
/// `catalogue.json.minisig` like the simulator catalogue's. A set's file,
/// once published, never changes; `latest` is published again daily under a
/// new name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DefinitionCatalogue {
    /// Bumped only for incompatible changes to this format.
    pub schema: u32,
    /// RFC 3339. A daemon refuses one older than the newest it has seen.
    pub published_at: String,
    pub sets: Vec<CatalogueSet>,
}

pub const DEFINITION_CATALOGUE_SCHEMA: u32 = 1;

/// One firmware version's definitions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CatalogueSet {
    /// Such as `arducopter-4.5.7`.
    pub id: String,
    pub firmware: Firmware,
    /// `4.5.7`, `4.6.0-beta2`, or `latest`.
    pub version: String,
    /// The full git hash of the tag it was generated from.
    pub commit: String,
    /// The gzipped set.
    pub file: CatalogueFile,
    /// ArduPilot's source at that commit, under the GPL; the set is
    /// generated from its comments.
    pub source_url: String,
}

/// How well a vehicle's set fits what it runs (P3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Match {
    /// A set the person added, bound to this vehicle, its commit or its
    /// firmware.
    Own,
    /// Generated from the commit the vehicle was built from.
    Commit,
    /// The release the vehicle runs.
    Exact,
    /// Another release, or the development build's: close, not certain.
    Approximate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum DefinitionsStatus {
    Loading,
    Ready,
    /// None fits: names and values only.
    Missing,
    /// The chosen set could not be had: names and values only.
    Failed,
}

/// Which set describes a vehicle's parameters; keyed by vehicle id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleDefinitions {
    pub vehicle_id: String,
    pub status: DefinitionsStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_id: Option<String>,
    /// "Copter 4.5.7", or the label of a set a person added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched: Option<Match>,
    /// One quiet line for the Parameters tab: an approximate set, or why
    /// there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A set a person added (P5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OwnSet {
    pub id: String,
    pub label: String,
    pub firmware: Firmware,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Describes vehicles built from this commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Describes this vehicle. With neither, every vehicle of its firmware.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    pub parameter_count: u32,
    #[schemars(schema_with = "crate::safe::integer")]
    pub added_at_us: u64,
    /// Where a CI publishes it, fetched again to stay current (S74).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[schemars(schema_with = "crate::safe::optional_integer")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at_us: Option<u64>,
}

/// `param/definitions`: what a vehicle's parameters mean.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamDefinitionsParams {
    pub vehicle_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamDefinitionsResult {
    /// Absent until UXDuck has looked for one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<VehicleDefinitions>,
    /// One per parameter the vehicle reports that the set describes, the
    /// variant for this vehicle's frame. Empty until the set is ready.
    pub definitions: Vec<ParamDefinition>,
}

/// `param/definitions/list`; `refresh` fetches the catalogue again.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DefinitionsListParams {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub refresh: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CatalogueState {
    /// Not asked for yet: nothing has needed it.
    Unknown,
    Loading,
    Ready,
    Failed,
    /// This UXDuck cannot download definitions.
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DefinitionsListResult {
    pub own: Vec<OwnSet>,
    pub catalogue: CatalogueState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalogue_detail: Option<String>,
    /// Sets the catalogue lists.
    pub catalogue_sets: u32,
}

/// `param/definitions/add`: a set from a file's text, which is ArduPilot's
/// `apm.pdef.json` or `apm.pdef.xml`, or a UXDuck set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DefinitionsAddParams {
    /// The file's text. Give this or `url`.
    #[serde(default)]
    pub text: String,
    /// Where a CI publishes the set, `https://`: fetched now, and again
    /// every day and on a refresh (S74). Give this or `text`; with it,
    /// `firmware` is needed when the file does not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Its name; defaults to the firmware and version or commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Needed when the file does not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<Firmware>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Describe vehicles built from this commit (at least 7 hex characters).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Describe this vehicle only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
}

/// `param/search`: parameters by what they mean (P8). With no vehicle,
/// the only one heard is meant.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamSearchParams {
    /// Words, such as "battery failsafe", or a name or its start, such as
    /// `RTL_`.
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    /// Search the vehicle's whole definition set, including parameters it
    /// does not report, rather than only those it does.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub whole_set: bool,
    /// Only those changed from their default (S74), where it is known.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub changed_only: bool,
    /// At most this many hits (default 20, at most 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
}

/// One parameter, in brief.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamBrief {
    pub name: String,
    /// Absent when its set does not describe it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The description's first sentence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Absent when the vehicle does not report it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units: Option<String>,
    /// The firmware's default, when the vehicle said it (S74).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<f64>,
    /// Changed from that default.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub changed: bool,
    /// The value's name, such as `RTL`, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_name: Option<String>,
}

/// Which set the answer is from, as the agent must say (P8).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DescribedBy {
    pub vehicle_id: String,
    pub vehicle_name: String,
    /// What the vehicle runs, such as `4.5.8`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
    /// Absent when no set describes this vehicle: names and values only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definitions: Option<VehicleDefinitions>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamSearchResult {
    pub described_by: DescribedBy,
    pub hits: Vec<ParamBrief>,
    /// Matches in all, before paging.
    pub total: u32,
}

/// `param/get`: parameters in full, by name.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamGetParams {
    pub names: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
}

/// One parameter: its value and everything its set says.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamDetail {
    pub name: String,
    /// Absent when the vehicle does not report it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// The firmware's default, when the vehicle said it (S74).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<f64>,
    /// Changed from that default.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub changed: bool,
    /// Absent when its set does not describe it: say so, never guess.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<ParamDefinition>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamGetResult {
    pub described_by: DescribedBy,
    pub parameters: Vec<ParamDetail>,
}

/// `param/list`: a vehicle's parameters by group.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    /// A prefix, such as `BATT_` or `RTL_`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Only those ArduPilot marks Standard.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub standard_only: bool,
    /// Only those changed from their default (S74), where it is known.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub changed_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GroupCount {
    pub group: String,
    pub count: u32,
}

/// Parameters, or, with no group asked for and many to show, how many
/// each group has: ask again for the group needed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamListResult {
    pub described_by: DescribedBy,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<ParamBrief>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<GroupCount>,
    pub total: u32,
}

/// `param/diff`: a vehicle's values against a `.param` file's, another
/// vehicle's or a snapshot's (P10). Give one of them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamDiffParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    /// A `.param` file: `NAME,value` or `NAME value` lines, `#` comments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_vehicle_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
    /// A baseline set for the vehicle (C7), from `param/baselines`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamDifference {
    pub name: String,
    /// The vehicle's; absent when it does not have it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// The other side's; absent when it does not have it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theirs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamDiffResult {
    pub described_by: DescribedBy,
    pub differences: Vec<ParamDifference>,
    /// Lines of the file that were not `NAME,value`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unread_lines: Vec<u32>,
}

/// A vehicle's named, pinned parameter set (C7).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamBaseline {
    pub id: String,
    pub vehicle_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    pub set_by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// How many parameters it pins.
    pub count: u32,
    /// How many of the vehicle's parameters differ from it now.
    pub differ: u32,
    #[schemars(schema_with = "crate::safe::integer")]
    pub created_at_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

/// `param/baselineSet`: the vehicle's parameters now, pinned as a baseline.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamBaselineSetParams {
    pub vehicle_id: String,
    /// "Approved" and the date, unless named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// `param/baselines`: a vehicle's baselines, newest first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamBaselinesParams {
    pub vehicle_id: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamBaselinesResult {
    pub baselines: Vec<ParamBaseline>,
}

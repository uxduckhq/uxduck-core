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
    /// In an answer about a vehicle, the set that described it there
    /// (ADR-0044 F9): the person's, such as "Acme additions", or
    /// ArduPilot's, such as "Copter 4.5.7". Never in a set's own file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
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

/// Which of one kind of set describes a vehicle (ADR-0044 F10).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case", tag = "type")]
pub enum SetChoice {
    /// ArduPilot's by the vehicle's version and commit; the person's by
    /// what their sets are bound to.
    #[default]
    Automatic,
    /// This set, by id, whatever the vehicle runs.
    Set { id: String },
    /// None of this kind.
    None,
}

impl SetChoice {
    #[must_use]
    pub fn is_automatic(&self) -> bool {
        *self == Self::Automatic
    }
}

/// The sets chosen for one vehicle; absent ones are automatic.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DefinitionsChoice {
    #[serde(default, skip_serializing_if = "SetChoice::is_automatic")]
    pub ardupilot: SetChoice,
    #[serde(default, skip_serializing_if = "SetChoice::is_automatic")]
    pub own: SetChoice,
}

impl DefinitionsChoice {
    #[must_use]
    pub fn is_automatic(&self) -> bool {
        self.ardupilot.is_automatic() && self.own.is_automatic()
    }
}

/// Which sets describe a vehicle's parameters: ArduPilot's, and the
/// person's read over it (ADR-0044 F9); keyed by vehicle id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleDefinitions {
    pub vehicle_id: String,
    pub status: DefinitionsStatus,
    /// The firmware the vehicle runs, whose sets can be chosen for it with
    /// `param/definitions/use`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<Firmware>,
    /// ArduPilot's set, from the catalogue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_id: Option<String>,
    /// Its name, such as "Copter 4.5.7".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// How well ArduPilot's set fits what the vehicle runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched: Option<Match>,
    /// The person's set, whose definitions win by name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own_set_id: Option<String>,
    /// Its label, such as "Acme additions".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own_title: Option<String>,
    /// What was chosen for this vehicle with `param/definitions/use`.
    #[serde(default, skip_serializing_if = "DefinitionsChoice::is_automatic")]
    pub choice: DefinitionsChoice,
    /// One quiet line for the Parameters tab: the sets used ("Descriptions
    /// from Acme additions over Copter 4.5.7."), an approximate set, or why
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
    /// The firmware profile it came with (S254): removed with it, not on
    /// its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
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
    /// Also list the catalogue's sets for this firmware, to choose one for
    /// a vehicle with `param/definitions/use`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<Firmware>,
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
    /// With `firmware`: the catalogue's sets for it, `latest` first, then
    /// the newest release first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sets: Vec<CatalogueSet>,
}

/// `param/definitions/use`: which sets describe a vehicle (ADR-0044 F10).
/// A kind left out stays as it was.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DefinitionsUseParams {
    pub vehicle_id: String,
    /// ArduPilot's: automatic, one of the catalogue's sets for the
    /// vehicle's firmware (see `param/definitions/list` with `firmware`),
    /// or none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ardupilot: Option<SetChoice>,
    /// The person's, read over ArduPilot's: automatic, one of their sets
    /// for the vehicle's firmware, or none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own: Option<SetChoice>,
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
    /// The set that described it (ADR-0044 F9), such as "Acme additions"
    /// or "Copter 4.5.7"; absent when undescribed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// A statistic (`STAT_*`, or read-only): it changes as the vehicle
    /// runs, not because anyone set it (ADR-0050 L8).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub statistic: bool,
    /// On a replay vehicle, how its log knows this value (ADR-0050 L5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_log: Option<ParamInLog>,
}

/// How a replay's log knows a value at the time asked (ADR-0050 L5, L9).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamInLog {
    pub known: ParamKnown,
    /// When the log read this value (µs since the Unix epoch).
    #[schemars(schema_with = "crate::safe::integer")]
    pub read_at_us: u64,
    /// When the log next has it change, if it does: the value differs then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub changes_at_us: Option<u64>,
}

/// `seen`: read by that time. `later`: first read after it, with no
/// change recorded between, so it is the value then as far as the log says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ParamKnown {
    Seen,
    Later,
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
    /// A replay of a flight (ADR-0039 R3): its values are only those its
    /// log recorded, and nothing can be downloaded from it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replay: bool,
    /// How many of the vehicle's parameters UXDuck holds, and whether they
    /// are all of them (FB-104).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<HeldParameters>,
}

/// What UXDuck holds of a vehicle's parameters, so an answer is never
/// taken for the whole set when it is not (FB-104).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeldParameters {
    /// Where its download stands.
    pub status: crate::dto::parameter::TransferStatus,
    /// How many UXDuck holds.
    pub count: u32,
    /// How many the vehicle has, as a running download says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u32>,
    /// Every parameter the vehicle had when they were last downloaded.
    pub complete: bool,
    /// When they were last downloaded (µs since the Unix epoch).
    #[schemars(schema_with = "crate::safe::optional_integer")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced_at_us: Option<u64>,
    /// What these values are and how to get the rest, when they are not
    /// all of them or not current.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// For a replay: the time on its clock these counts are as at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub at_us: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamSearchResult {
    pub described_by: DescribedBy,
    pub hits: Vec<ParamBrief>,
    /// Matches in all, before paging.
    pub total: u32,
    /// Where the next page begins (`offset`), when there are more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<u32>,
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
    /// A statistic, not a setting (ADR-0050 L8).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub statistic: bool,
    /// On a replay vehicle, how its log knows this value (ADR-0050 L5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_log: Option<ParamInLog>,
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
    /// A replay vehicle's values at this time on its clock (µs), rather
    /// than at the replay's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub at_us: Option<u64>,
}

/// `param/changes`: a replay vehicle's parameter changes in its log
/// (ADR-0050 L9).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamChangesParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    /// From this time on the replay's clock (µs); else the log's start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub from_us: Option<u64>,
    /// To this time; else the log's end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub to_us: Option<u64>,
    /// Statistics' changes too (`STAT_*`), left out otherwise.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub statistics: bool,
}

/// Who made a change (ADR-0050 L5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ParamChangeBy {
    /// UXDuck set it.
    Uxduck,
    /// Another ground station, `systemId`, set it.
    GroundStation,
    /// The vehicle reported it by itself.
    Vehicle,
    /// Two downloads differ with no change seen between: made over another
    /// link, or lost on this one.
    Unseen,
}

/// A value that differs from the one read before it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamChange {
    pub name: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub at_us: u64,
    pub from: f64,
    pub to: f64,
    pub by: ParamChangeBy,
    /// The ground station's, for `groundStation`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_id: Option<u8>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub statistic: bool,
    /// Why, as typed when UXDuck wrote it on this computer (S74): from the
    /// vehicle's history, which a log from elsewhere does not have.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A parameter not read by the replay's time, with when the log first
/// read it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamSeenLater {
    pub name: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub read_at_us: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamChangesResult {
    pub described_by: DescribedBy,
    /// Oldest first.
    pub changes: Vec<ParamChange>,
    /// How many statistics' changes were left out.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub statistics_left_out: u32,
    /// The values not read by the replay's time, known from later.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub later: Vec<ParamSeenLater>,
}

#[allow(clippy::trivially_copy_pass_by_ref, reason = "serde's signature")]
fn is_zero(n: &u32) -> bool {
    *n == 0
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
    /// Where the next page begins (`offset`), when there are more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<u32>,
}

/// `param/diff`: a vehicle's values against a `.param` file's, another
/// vehicle's, a snapshot's or a saved set's (P10). Give one of them.
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
    /// A saved parameter set of the vehicle's (S217).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_id: Option<String>,
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

/// How a saved parameter set came to be (S217).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SavedParamsOrigin {
    /// Saved from the vehicle's values.
    Saved,
    /// A `.param` file imported: kept before anything was written.
    Imported,
    /// Saved automatically before another set was applied ("Before Field
    /// tune").
    Before,
    /// Saved automatically before a firmware install.
    Firmware,
    /// A tuning snapshot (S329, ADR-0054 T5): only the vehicle's tuning
    /// parameters, so applying it (a jump) touches nothing else.
    Tuning,
}

/// A saved parameter set (S217, ADR-0037 V19): all of a vehicle's
/// parameters exactly as read, kept as a `.param` file in the vehicle's
/// Files on this computer, never synced to the account. Only ever saved,
/// applied, renamed or deleted, never edited.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SavedParams {
    pub id: String,
    pub vehicle_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub origin: SavedParamsOrigin,
    /// Who saved it, by name at the time.
    pub saved_by: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub saved_at_us: u64,
    /// The vehicle's firmware when it was saved; absent for an import.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
    /// How many parameters it holds.
    pub count: u32,
    /// How many of the vehicle's parameters differ from it now: 0 when the
    /// vehicle matches it. Absent before the vehicle's parameters are read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub differ: Option<u32>,
    /// Its file's name in "Vehicle files › Parameters"; absent when this
    /// UXDuck keeps no files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// `param/sets`: a vehicle's saved parameter sets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamSetsParams {
    pub vehicle_id: String,
}

/// A vehicle's saved sets, newest first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamSetsResult {
    pub sets: Vec<SavedParams>,
}

/// `param/setSave`: keeps the vehicle's parameters as they are now under a
/// name, or, with `text`, a `.param` file as it is (Import), before
/// anything is written.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamSetSaveParams {
    pub vehicle_id: String,
    /// Up to 200 characters; "Saved <date>" or the file's name by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// A `.param` file: `NAME,value` or `NAME value` lines, `#` comments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Only its tuning parameters (S329); one matching is answered instead.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tuning: bool,
}

/// `param/setApply`: writes every parameter that differs from a saved
/// set, as one change with the reason "Applied <set>", after saving the
/// vehicle's values as "Before <set>". Answered once every write has.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamSetApplyParams {
    pub vehicle_id: String,
    pub id: String,
    /// Confirms writing to a vehicle that is not online.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub confirm: bool,
}

/// Why a set's parameter was not written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SkipReason {
    /// The vehicle does not have it.
    NotOnVehicle,
    /// Its definition says it is read-only.
    ReadOnly,
    /// A statistic the vehicle keeps itself (`STAT_`).
    Statistic,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkippedParam {
    pub name: String,
    pub why: SkipReason,
}

/// A write the vehicle did not take, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RefusedParam {
    pub name: String,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamSetApplyResult {
    /// The vehicle's values saved first; absent when nothing differed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<SavedParams>,
    /// Written and confirmed, by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub written: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refused: Vec<RefusedParam>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedParam>,
    /// Written ones that take effect once the vehicle restarts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reboot_required: Vec<String>,
}

/// `param/setRename`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamSetRenameParams {
    pub id: String,
    pub name: String,
    /// Its note too, when given; empty clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// `param/setExport`'s result: the set as a `.param` file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamSetExportResult {
    /// A file name for it, such as `Alpha-Field-tune.param`.
    pub name: String,
    pub text: String,
}

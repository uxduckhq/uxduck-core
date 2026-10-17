//! Parameters on the wire (ADR-0004, ADR-0001 D7). The autopilot holds the
//! truth; a write is a compare-and-swap on the value the client last saw.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `MAV_PARAM_TYPE`: how the autopilot stores a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ParamKind {
    Uint8,
    Int8,
    Uint16,
    Int16,
    Uint32,
    Int32,
    Uint64,
    Int64,
    Real32,
    Real64,
}

/// One parameter of one vehicle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Parameter {
    /// `<vehicle id>/<NAME>`.
    pub id: String,
    pub vehicle_id: String,
    pub name: String,
    pub value: f64,
    pub kind: ParamKind,
    /// The firmware's default, when the vehicle said it (S74).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<f64>,
}

/// Where a download or upload stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TransferStatus {
    /// Nothing downloaded yet.
    Unknown,
    Downloading,
    Uploading,
    /// Matches the vehicle as of its current boot.
    Synced,
    /// `detail` says why.
    Failed,
}

/// A vehicle's parameter set: its status, not its values (those are
/// `parameter` entities). Keyed by `vehicleId`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParameterSet {
    pub vehicle_id: String,
    pub status: TransferStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub count: u32,
    /// When the autopilot booted (µs since the Unix epoch, estimated): the
    /// set belongs to that boot.
    #[schemars(schema_with = "crate::safe::optional_integer")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_instant_us: Option<u64>,
    #[schemars(schema_with = "crate::safe::optional_integer")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced_at_us: Option<u64>,
    /// How far a download has come while it runs (S127).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<TransferProgress>,
}

/// Sets one parameter, as a compare-and-swap: refused with
/// `version-conflict` if the value is no longer `expectedValue`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamSetParams {
    pub vehicle_id: String,
    pub name: String,
    pub value: f64,
    /// The value the client last saw; omitted, the write is unconditional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_value: Option<f64>,
    /// Confirms a write to a vehicle that is not online (parameter writes
    /// are `warn` in the offline table).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub confirm: bool,
    /// Why, up to 200 characters, kept with the change in the vehicle's
    /// history (S74), such as "higher hills".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Sent with the other writes asked for in the same tenth of a second
    /// as one parameter file, when they are eight or more and the vehicle
    /// is ArduPilot, which takes them that way (S121, ADR-0027 F13):
    /// quicker over a radio. Confirmed by reading the file back; anything
    /// not confirmed goes again one by one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub by_file: bool,
}

/// How a write to the vehicle ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum WriteStatus {
    /// The vehicle confirmed exactly what was asked.
    Accepted,
    /// The vehicle answered with something else; `detail` says what.
    Rejected,
    /// No answer after every retry.
    Timeout,
    /// Never sent; `detail` says why.
    NotSent,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamSetResult {
    pub status: WriteStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub requested: f64,
    /// The parameter as the vehicle now reports it (or as last known when
    /// it did not answer).
    pub parameter: Parameter,
    /// Its units, from the vehicle's definition set (S68).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units: Option<String>,
    /// A new value takes effect only after the autopilot reboots.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reboot_required: bool,
}

/// How far a transfer has come (S127): what is done of the total, which
/// try this is (above 1 when the vehicle went quiet and it asked again),
/// and when it last moved, so a stall can be shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TransferProgress {
    pub done: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u32>,
    pub attempt: u8,
    #[schemars(schema_with = "crate::safe::integer")]
    pub at_us: u64,
    #[serde(default)]
    pub upload: bool,
}

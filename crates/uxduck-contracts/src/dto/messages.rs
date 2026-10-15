//! A vehicle's own messages (`STATUSTEXT`, S26, ADR-0004): every one kept in
//! the vehicle's log, a run of repeats folded into one entry with a count.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One entry of a vehicle's log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleMessage {
    /// Unique in this run of UXDuck, rising.
    #[schemars(schema_with = "crate::safe::integer")]
    pub id: u64,
    pub vehicle_id: String,
    pub component_id: u8,
    /// `MAV_SEVERITY`: 0 emergency, 1 alert, 2 critical, 3 error, 4 warning,
    /// 5 notice, 6 info, 7 debug.
    pub severity: u8,
    pub text: String,
    /// When it first came, and when last (microseconds since 1970).
    #[schemars(schema_with = "crate::safe::integer")]
    pub first_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub last_us: u64,
    /// How many times it came within the quiet window: 1 for once.
    pub count: u32,
}

/// `vehicle/messages`: a vehicle's log, oldest first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleMessagesParams {
    pub vehicle_id: String,
    /// Only this severity and more severe (0 is the most); all if left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_severity: Option<u8>,
    /// Only messages whose text holds these words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Every message as it came, repeats too, rather than the folded log.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub raw: bool,
    /// The newest this many; 200 if left out, at most 2000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Also send `vehicle/message` as entries come or change.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub subscribe: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleMessagesResult {
    pub messages: Vec<VehicleMessage>,
}

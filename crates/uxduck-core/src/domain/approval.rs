//! Approvals (ADR-0005): a non-operator client's action that needs a
//! capability it does not hold is held here and put to an operator instead
//! of failing. Every approval has a deadline and a safe default: deny.

use serde::{Deserialize, Serialize};

use crate::domain::client::Capability;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalState {
    Pending,
    Allowed,
    Denied,
    /// Nobody answered by the deadline: denied, the safe default.
    Expired,
}

/// How long an allowance lasts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Remember {
    /// This call only.
    Once,
    /// For the rest of the client's connection.
    Connection,
    /// Stored in the client's capability set.
    Always,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Approval {
    pub id: String,
    /// The client record asking.
    pub client_id: String,
    pub client_name: String,
    pub capability: Capability,
    /// The API method, such as `command/send`.
    pub method: String,
    /// For people: "arm Copter 1".
    pub summary: String,
    pub state: ApprovalState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remember: Option<Remember>,
    pub deadline_us: u64,
    pub created_at_us: u64,
    pub version: u64,
}

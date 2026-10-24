//! Cloud sync on the wire (S16, ADR-0001 D9, ADR-0002): whether this
//! computer mirrors its chats, places, operators and vehicles to the
//! account, and the vehicles other computers published there.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Which vehicles from the account this computer uses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SyncActivation {
    /// Only the ones someone chooses here.
    #[default]
    Pick,
    /// Every one, as it arrives.
    MirrorAll,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatus {
    /// Whether this UXDuck can sync: an installed one with an account server.
    pub available: bool,
    pub enabled: bool,
    pub activation: SyncActivation,
    /// Changes sent to the account so far.
    #[schemars(schema_with = "crate::safe::integer")]
    pub exported: u64,
    /// Changes taken from other computers so far.
    #[schemars(schema_with = "crate::safe::integer")]
    pub pulled: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub last_synced_at_us: Option<u64>,
    /// Why the last attempt failed, such as not being signed in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    /// Vehicles other computers published to the account.
    pub library: Vec<LibraryVehicle>,
    /// Connections other computers published to the account (C2).
    #[serde(default)]
    pub links: Vec<LibraryLink>,
}

/// A connection another computer published to the account (C2): how to
/// reach its vehicles. Signing keys never travel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LibraryLink {
    pub id: String,
    pub name: String,
    pub kind: crate::dto::link::LinkKind,
    /// The computer that published it, by name.
    pub device: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub updated_at_us: u64,
    /// Used on this computer.
    pub active: bool,
}

/// What `sync/activate` uses here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SyncEntity {
    #[default]
    Vehicle,
    Link,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LibraryVehicle {
    pub id: String,
    pub name: String,
    pub autopilot: String,
    pub vehicle_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
    /// The computer that published it, by name.
    pub device: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub updated_at_us: u64,
    /// Used on this computer.
    pub active: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SyncSetParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation: Option<SyncActivation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SyncActivateParams {
    /// A vehicle, or with `entity: link` a connection, in the library.
    pub id: String,
    #[serde(default, skip_serializing_if = "is_vehicle")]
    pub entity: SyncEntity,
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes a reference"
)]
fn is_vehicle(entity: &SyncEntity) -> bool {
    *entity == SyncEntity::Vehicle
}

/// `account/versions`: one synced entity's copies in the account (C5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccountVersionsParams {
    /// `plan`, `operator`, `link`, `thread` and the rest, as synced.
    pub entity: String,
    pub id: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccountVersions {
    pub versions: Vec<AccountVersion>,
}

/// One copy of an entity the account was sent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccountVersion {
    /// The computer that sent it, by name.
    pub device: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
    pub digest: String,
    #[schemars(schema_with = "crate::safe::integer")]
    pub ts_us: u64,
    /// Whether it became the account's copy.
    pub won: bool,
    /// The journal change as that computer sent it.
    pub change: serde_json::Value,
}

/// `sync/restore`: an earlier version of a plan, as `account/versions` gave it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SyncRestoreParams {
    pub id: String,
    /// The version's `change`, a `planSaved` change for this plan.
    pub change: serde_json::Value,
}

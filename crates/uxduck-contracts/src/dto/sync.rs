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

/// What a person can keep off the account; sync is everything until they
/// leave something out. Flights go only when someone saves them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SyncCategory {
    /// Operators with their display settings and layouts, and published plugins.
    Settings,
    Chats,
    Places,
    /// Saved vehicles and their history, snapshots, connections.
    Vehicles,
    Plans,
}

impl From<uxduck_core::domain::sync::SyncCategory> for SyncCategory {
    fn from(category: uxduck_core::domain::sync::SyncCategory) -> Self {
        use uxduck_core::domain::sync::SyncCategory as Core;
        match category {
            Core::Settings => Self::Settings,
            Core::Chats => Self::Chats,
            Core::Places => Self::Places,
            Core::Vehicles => Self::Vehicles,
            Core::Plans => Self::Plans,
        }
    }
}

impl From<SyncCategory> for uxduck_core::domain::sync::SyncCategory {
    fn from(category: SyncCategory) -> Self {
        match category {
            SyncCategory::Settings => Self::Settings,
            SyncCategory::Chats => Self::Chats,
            SyncCategory::Places => Self::Places,
            SyncCategory::Vehicles => Self::Vehicles,
            SyncCategory::Plans => Self::Plans,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatus {
    /// Whether this UXDuck can sync: an installed one with an account server.
    pub available: bool,
    pub enabled: bool,
    pub activation: SyncActivation,
    /// What stays off the account; empty syncs everything.
    #[serde(default)]
    pub skipped: Vec<SyncCategory>,
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
    /// What the account would not keep for its size. Each stays on this
    /// computer until it is smaller or the account has room for it. Empty
    /// when everything fits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub too_large: Vec<SyncTooLarge>,
    /// Cameras someone added that stay on this computer, and why: an
    /// address whose login's end cannot be told, or more than a camera's
    /// copy in the account holds (FB-229). Empty when every one syncs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kept_here: Vec<SyncKeptHere>,
    /// Vehicles other computers published to the account.
    pub library: Vec<LibraryVehicle>,
    /// Connections other computers published to the account (C2).
    #[serde(default)]
    pub links: Vec<LibraryLink>,
    /// Cameras other computers published to the account (FB-229).
    #[serde(default)]
    pub cameras: Vec<LibraryCamera>,
}

/// Something whose latest change the account would not keep for its size,
/// so it is on this computer only: larger than the account takes at once,
/// or more than the account has room for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SyncTooLarge {
    /// `plan`, `operator`, `history` and the rest, as synced.
    pub entity: String,
    pub id: String,
    /// What did not sync, by its name to the person, such as "The plan
    /// Survey North did not sync".
    pub title: String,
    /// Its size, what the account takes or has room for, and what to do,
    /// in words.
    pub detail: String,
    /// The change's size as the account measured it.
    #[schemars(schema_with = "crate::safe::integer")]
    pub bytes: u64,
    /// The largest change the account takes at once, when its server said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub max_bytes: Option<u64>,
    /// Set when the account had no room for it. It is then not too large
    /// to take, and syncs once the account has room.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_room: Option<SyncNoRoom>,
}

/// Something that stays on this computer and never goes to the account
/// while it is as it is (FB-229).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SyncKeptHere {
    /// `camera`.
    pub entity: String,
    pub id: String,
    /// What stays, by its name, such as "The camera Front stays on this
    /// computer".
    pub title: String,
    /// Why, and what to change for it to sync, in words.
    pub detail: String,
}

/// Why the account had no room for a change, in its server's numbers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SyncNoRoom {
    /// What keeping the change would add to the account: its copy among
    /// the kept versions, and as the entity's copy what it is larger by
    /// than the one it replaces.
    #[schemars(schema_with = "crate::safe::integer")]
    pub adds_bytes: u64,
    /// What the account held then.
    #[schemars(schema_with = "crate::safe::integer")]
    pub account_bytes: u64,
    /// What the account's plan allows.
    #[schemars(schema_with = "crate::safe::integer")]
    pub allowance_bytes: u64,
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

/// A camera another computer published to the account (FB-229): how to
/// add it here. Its login never travels.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LibraryCamera {
    pub id: String,
    pub name: String,
    /// The vehicle it was given there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
    /// Its RTSP or WHEP addresses, preferred first, without a login.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub urls: Vec<String>,
    /// For a camera that sends to UXDuck, the UDP port it listens on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udp_port: Option<u16>,
    /// An address had a login there, so `sync/activate` takes one typed
    /// here.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub needs_login: bool,
    /// An address had a token, a key or a signature there, which no user
    /// name or password stands for, so its address is edited here.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub needs_address: bool,
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
    Camera,
}

/// The login a camera from the account needs on this computer (FB-229).
/// It is kept here and never goes to the account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CameraLogin {
    pub user: String,
    #[serde(default)]
    pub password: String,
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
    /// Its size on the map, as the person set it, when they did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_cm: Option<u32>,
    /// The colour the person gave it, when they did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub colour: Option<super::vehicle::VehicleColour>,
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
    /// What to keep off the account from now on; empty syncs everything.
    /// Bringing something back sends and takes what it missed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<Vec<SyncCategory>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SyncActivateParams {
    /// A vehicle, or with `entity: link` a connection, or with
    /// `entity: camera` a camera, in the library.
    pub id: String,
    #[serde(default, skip_serializing_if = "is_vehicle")]
    pub entity: SyncEntity,
    /// For a camera that needs one, its login on this computer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<CameraLogin>,
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

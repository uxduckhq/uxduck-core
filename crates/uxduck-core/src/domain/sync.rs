//! Cloud sync (ADR-0001 D9, ADR-0002, S16): whether this computer mirrors
//! its work to the account, how far it has got each way, and the account's
//! library of vehicles published by other computers.

use serde::{Deserialize, Serialize};

/// Which vehicles from the account this computer uses (ADR-0001 D9,
/// "activation per device").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Activation {
    /// Only the ones someone chooses here.
    #[default]
    Pick,
    /// Every one, as it arrives.
    MirrorAll,
}

/// What a person can choose to keep off the account (2026-10-24): sync is
/// everything until they leave something out. Flights are not here: each
/// goes only when someone saves it (C9).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SyncCategory {
    /// Operators with their display settings and layouts, and plugins
    /// they published.
    Settings,
    /// Chats and their folders.
    Chats,
    Places,
    /// Saved vehicles, their history and snapshots, connections
    /// and parameter definition sets.
    Vehicles,
    Plans,
}

impl SyncCategory {
    /// The category an entity belongs to; none for one that is always the
    /// person's own choice, item by item.
    #[must_use]
    pub fn of(entity: &str) -> Option<Self> {
        Some(match entity {
            "operator" | "plugin" => Self::Settings,
            "thread" | "item" | "folder" => Self::Chats,
            "place" => Self::Places,
            "vehicle" | "history" | "snapshot" | "link" | "own-set" | "dialect"
            | "firmware-profile" => Self::Vehicles,
            "plan" => Self::Plans,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncState {
    /// On unless the person turned it off (maintainer, 2026-11-06: "lets
    /// auto sync it - not get people to opt in"). A computer turned off has
    /// a `device_id`, so it stays off; one from before, off without one,
    /// is turned on once.
    pub enabled: bool,
    #[serde(default)]
    pub activation: Activation,
    /// What stays off the account; empty syncs everything.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SyncCategory>,
    /// This computer in the account; minted when sync is first turned on.
    #[serde(default)]
    pub device_id: String,
    /// The export cursor: this computer's events up to here are in the
    /// account, or were not the account's to have.
    #[serde(default)]
    pub exported_through: u64,
    /// Changes sent so far.
    #[serde(default)]
    pub exported: u64,
    /// The account's cursor: changes from other computers taken up to here.
    #[serde(default)]
    pub pulled_through: u64,
    /// Changes taken so far.
    #[serde(default)]
    pub pulled: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_synced_at_us: Option<u64>,
    /// The account this browser's work belongs to, from its first sync
    /// (ADR-0033): another account signed in here does not receive it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub account_id: String,
    #[serde(default)]
    pub version: u64,
}

impl Default for SyncState {
    /// On from the start; its device id is minted when it is first needed.
    fn default() -> Self {
        Self {
            enabled: true,
            activation: Activation::default(),
            skipped: Vec::new(),
            device_id: String::new(),
            exported_through: 0,
            exported: 0,
            pulled_through: 0,
            pulled: 0,
            last_synced_at_us: None,
            account_id: String::new(),
            version: 0,
        }
    }
}

impl SyncState {
    /// Whether changes to this entity go to the account and come from it.
    #[must_use]
    pub fn syncs(&self, entity: &str) -> bool {
        SyncCategory::of(entity).is_none_or(|category| !self.skipped.contains(&category))
    }
}

/// A vehicle another computer published to the account: its profile, not
/// where it was last heard.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Its size on the map in centimetres, as the person gave it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_cm: Option<u32>,
    /// Its colour, as given or chosen (ADR-0029 P19), so it is the same on
    /// every computer of the account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub colour: Option<crate::domain::vehicle::VehicleColour>,
    /// Which parameter descriptions it uses, as a person chose (ADR-0044
    /// F10).
    #[serde(
        default,
        skip_serializing_if = "crate::domain::param_definitions::DefinitionsChoice::is_automatic"
    )]
    pub definitions: crate::domain::param_definitions::DefinitionsChoice,
    /// Which MAVLink dialects it uses, as a person chose (F13).
    #[serde(
        default,
        skip_serializing_if = "crate::domain::dialect::DialectChoice::is_automatic"
    )]
    pub dialects: crate::domain::dialect::DialectChoice,
    /// The computer that published this copy.
    pub device: String,
    /// When that computer changed it.
    pub updated_at_us: u64,
}

/// A connection another computer published to the account (C2): how to
/// reach its vehicles, not whether it is up. Used on a computer only once
/// someone there chooses it, or "Use them all" for a network link; a
/// serial link names its USB device. Signing keys never travel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryLink {
    pub id: String,
    pub name: String,
    pub kind: crate::domain::link::LinkKind,
    pub policy: crate::domain::link::LinkPolicy,
    pub topology: crate::domain::link::Topology,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vehicle_filter: Vec<String>,
    #[serde(default)]
    pub carries: crate::domain::link::LinkCarries,
    /// The computer that published this copy.
    pub device: String,
    /// When that computer changed it.
    pub updated_at_us: u64,
}

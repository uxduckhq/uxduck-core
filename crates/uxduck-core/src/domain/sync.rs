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

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncState {
    /// Off until someone turns it on (ADR-0002: the exporter is off by
    /// default).
    pub enabled: bool,
    #[serde(default)]
    pub activation: Activation,
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
    #[serde(default)]
    pub version: u64,
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

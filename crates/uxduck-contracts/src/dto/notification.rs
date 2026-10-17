//! Notifications on the wire (ADR-0005): one tray on every client.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
    Info,
    Warning,
    Alarm,
}

/// `transient` hides into the tray after a few seconds and stays unread;
/// `persistent` holds screen space until acknowledged or resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum NotificationKind {
    Persistent,
    Transient,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum NotificationState {
    Active,
    /// Seen: off the screen, read in the tray.
    Acknowledged,
    /// Dismissed, or its condition cleared.
    Resolved,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Subject {
    Vehicle {
        id: String,
    },
    Link {
        id: String,
    },
    Client {
        id: String,
    },
    /// A camera stream (ADR-0017 V14).
    VideoSource {
        id: String,
    },
    System,
}

/// A button on a notification: an API call, typed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum Action {
    /// Calls `link/enable`.
    EnableLink { link_id: String },
    /// Calls `approval/resolve`.
    ResolveApproval {
        approval_id: String,
        allow: bool,
        remember: crate::dto::client::Remember,
    },
    /// Opens a page in the browser; nothing is called.
    OpenUrl { url: String },
    /// Opens a Video tab showing this source; nothing is called.
    OpenVideo { source_id: String },
    /// Opens the Controllers tab, for a controller just plugged in; nothing
    /// is called.
    OpenControllers { controller_id: String },
    /// Opens the vehicle's Messages tab at this entry (S26); nothing is called.
    OpenMessages {
        vehicle_id: String,
        #[schemars(schema_with = "crate::safe::integer")]
        message_id: u64,
    },
    /// Calls `link/add` for a serial link to this device, found by its USB
    /// identity.
    ConnectSerial {
        device: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usb: Option<crate::dto::link::UsbId>,
    },
    /// Calls `pairing/approve`, or `pairing/deny` when `allow` is false, for
    /// a browser asking to connect, such as UXDuck on the web. A phone or
    /// tablet on the network is allowed with `access` (S97).
    AnswerPairing {
        code: String,
        allow: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        access: Option<crate::dto::pairing::NetworkAccess>,
    },
    /// Calls `lease/release`: takes control of a vehicle back from someone
    /// away (S80).
    TakeBack { vehicle_id: String },
    /// Calls `sim/restart`: starts a simulated vehicle again from where it
    /// started, after a crash.
    RestartSimulation { link_id: String },
    /// Calls `settings/update` with `vehicleTerrain`: answers and checks a
    /// vehicle from this dataset from now on (S119).
    UseTerrainDataset {
        vehicle_id: String,
        dataset: crate::dto::terrain::TerrainDataset,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NotificationAction {
    pub label: String,
    pub action: Action,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Notification {
    pub id: String,
    pub severity: Severity,
    pub kind: NotificationKind,
    pub state: NotificationState,
    pub subject: Subject,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<NotificationAction>,
    /// It resolves itself when this clears, such as `vehicle-offline:<id>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    /// Offers "Report this", filled in with this code: set only where
    /// UXDuck itself may have gone wrong, such as a command that wasn't sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<String>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub created_at_us: u64,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

//! Notifications (ADR-0005): daemon-owned, so every client shows the same
//! tray and dismissing one anywhere dismisses it everywhere. A transient one
//! hides into the tray after a few seconds and stays there unread; a
//! persistent one holds screen space until it is acknowledged or its
//! condition clears.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
    Info,
    Warning,
    Alarm,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NotificationKind {
    Persistent,
    Transient,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NotificationState {
    Active,
    /// Seen: a persistent one leaves the screen, a transient one is read.
    Acknowledged,
    /// Over: dismissed, or its condition cleared. Out of the tray.
    Resolved,
}

/// What a notification is about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

/// A button on a notification: an API call the operator can make from it.
/// Typed, so a notification cannot carry an arbitrary call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum Action {
    /// `link/enable`.
    EnableLink {
        link_id: String,
    },
    /// `approval/resolve`.
    ResolveApproval {
        approval_id: String,
        allow: bool,
        remember: crate::domain::approval::Remember,
    },
    /// Opens a page in the browser, such as the updates sign-up.
    OpenUrl {
        url: String,
    },
    OpenVideo {
        source_id: String,
    },
    /// The Controllers tab, for a controller just plugged in.
    OpenControllers {
        controller_id: String,
    },
    /// `flight/keep`: a flight just finished, saved to the account (C9);
    /// `always` saves every flight from now on too.
    KeepFlight {
        flight_id: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        always: bool,
    },
    /// The vehicle's Messages tab at an entry (S26).
    OpenMessages {
        vehicle_id: String,
        message_id: u64,
    },
    /// A serial link to add, for a flight controller just plugged in (S44).
    ConnectSerial {
        device: String,
        usb: Option<crate::domain::link::UsbId>,
    },
    /// `pairing/approve` or `pairing/deny` for a browser asking to connect.
    AnswerPairing {
        code: String,
        allow: bool,
        /// For a phone or tablet on the network: what it may do (S97).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        access: Option<crate::domain::client::NetworkAccess>,
    },
    /// `lease/release` at the computer: takes control of a vehicle back
    /// from someone away (S80).
    TakeBack {
        vehicle_id: String,
    },
    /// `sim/restart`: a simulated vehicle started again from where it
    /// started, after a crash.
    RestartSimulation {
        link_id: String,
    },
    /// `settings/update` with a vehicle's dataset (S119).
    UseTerrainDataset {
        vehicle_id: String,
        dataset: crate::domain::settings::TerrainDataset,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationAction {
    pub label: String,
    pub action: Action,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Resolves it by itself when the condition clears, such as
    /// `vehicle-offline:<id>` when the vehicle is heard again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    /// Offers "Report this", filled in with this code: set only where
    /// UXDuck itself may have gone wrong, such as a command that wasn't sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<String>,
    pub created_at_us: u64,
    pub version: u64,
}

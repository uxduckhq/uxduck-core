//! The tray (S64, ADR-0008 "The tray"): one icon, drawn by `uxduck-tray`,
//! whose menu UXDuck builds from its own state. The tray subscribes with
//! `tray/menu`, draws what it is sent, and says which item was chosen with
//! `tray/activate`; UXDuck answers with what the tray should do, since only
//! the tray runs in the person's desktop session.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How the icon looks: plain unless something is broken or waiting for an
/// answer (ADR-0009: warn only when broken).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TrayTone {
    Plain,
    Broken,
    Waiting,
}

/// One item of the menu.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TrayItem {
    /// What `tray/activate` is sent: `open`, `open-browser`, `docs`,
    /// `feedback` or `quit`.
    pub id: String,
    pub label: String,
    /// A line drawn above it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub separator_before: bool,
    /// A submenu; absent for a clickable leaf.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<TrayItem>,
    /// Disabled entries remain visible, such as opening a starting vehicle.
    #[serde(default = "enabled", skip_serializing_if = "is_enabled")]
    pub enabled: bool,
    /// A switch, ticked or not (S108: "Open at login"); absent for an
    /// action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
}

fn enabled() -> bool {
    true
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde callback signature"
)]
fn is_enabled(value: &bool) -> bool {
    *value
}

/// The whole menu, as `tray/menu` answers and `tray/menuChanged` sends.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TrayMenu {
    /// The first line, not clickable: "UXDuck is running", the vehicles
    /// online, or what is wrong.
    pub status: String,
    pub tone: TrayTone,
    pub items: Vec<TrayItem>,
    /// An unattended-simulation reminder, also delivered while no UI is open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simulation_reminder: Option<SimulationReminder>,
    /// A vehicle's warning or alarm raised while no UXDuck window is
    /// visible, shown as the system's notification; the newest when there
    /// are more. Clicking it opens UXDuck.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert: Option<TrayAlert>,
    /// A question shown under the duck until it is answered or goes away,
    /// such as a browser asking to connect; the newest when there are more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<TrayQuestion>,
}

/// A question under the duck. Its answers are items: choosing one sends
/// `tray/activate` with its id, as the menu does.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TrayQuestion {
    /// Stays the same while the question does; a new id is a new question.
    pub id: String,
    pub title: String,
    pub message: String,
    /// The buttons, the last one the default.
    pub answers: Vec<TrayItem>,
}

/// A vehicle's warning for the system's notifications, by its notification's id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TrayAlert {
    pub id: String,
    pub title: String,
    pub message: String,
}

/// A native notification whose opaque token limits its actions to one snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SimulationReminder {
    /// An opaque token: actions apply only to this reminder's simulations.
    pub id: String,
    pub title: String,
    pub message: String,
}

/// `tray/activate`: the item chosen. For `feedback`, the screen as the tray
/// captured it, base64 PNG or JPEG, when it could.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TrayActivateParams {
    pub item: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<String>,
}

/// What the tray does next.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum TrayAction {
    /// Open this address in the default browser.
    OpenUrl { url: String },
    /// Start the desktop app with these arguments; a running one takes
    /// them and comes to the front.
    Launch { path: String, args: Vec<String> },
    /// Show this folder in Finder, Explorer or the file manager (S108:
    /// "Show files").
    Reveal { path: String },
    /// Nothing: UXDuck did it, such as quitting.
    Done,
    /// Ask first, in a dialog: `message` with `confirm` and Cancel. On
    /// `confirm`, send `tray/activate` again with `item` (S107: "Quit
    /// UXDuck" while something would stop).
    Confirm {
        title: String,
        message: String,
        confirm: String,
        item: String,
    },
}

/// `feedback/screenshot`: the screen the tray captured for `Send
/// feedback…`, kept for a few minutes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackScreenshotParams {
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackScreenshot {
    /// A data URL, or none when it was taken already or has expired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<String>,
}

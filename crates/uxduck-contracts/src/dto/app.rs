//! Opening UXDuck and quitting it (S107, ADR-0025 D1): which pages are
//! open and seen, bringing one forward instead of opening another, and what
//! would stop if UXDuck quit.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `client/visibility`: whether the page is on screen, sent when it
/// connects and on every `visibilitychange`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClientVisibilityParams {
    pub visible: bool,
}

/// `app/focus`: whether a connected page was asked to come forward. When
/// none was, nothing is open and the caller opens a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AppFocusResult {
    pub focused: bool,
}

/// Something that stops when UXDuck quits.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum StopKind {
    /// A simulation that is running or starting.
    Simulation,
    /// A link sending vehicles' traffic to another program.
    ForwardLink,
    /// A real vehicle that is connected.
    Vehicle,
    /// A camera being recorded.
    Recording,
}

/// How many of one kind would stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StopCount {
    pub kind: StopKind,
    pub count: u32,
}

/// `daemon/stopPreview`: what quitting would stop, and the words for it,
/// such as "2 simulations and a forward link will stop." No `summary` and
/// no `stops` when nothing would: quitting then asks nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StopPreview {
    pub stops: Vec<StopCount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// `app/open`: how UXDuck was opened. The desktop app was started (or
/// brought forward), a page already open came forward, or nothing was:
/// then `url` is the address to open in a browser, already paired.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AppOpenResult {
    pub opened: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// `app/open`: a page to open on, such as `/vehicles/sim-1`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AppOpenParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<String>,
    /// UXDuck was opened again while it ran (the Start menu, `uxduck
    /// open`): the tray says so under its icon with `tray/reopened`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reopened: bool,
}

/// Where the desktop app stands on this computer (S110, ADR-0025 D6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum DesktopAppState {
    /// Not offered here: no release names one for this computer, or this
    /// UXDuck cannot run one (a browser's own, no display).
    Unavailable,
    /// Asking the release channel.
    Checking,
    /// Offered, not downloaded.
    Available,
    Downloading,
    Installed,
    /// The last download or check failed; `detail` says why.
    Failed,
}

/// `desktopApp/status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DesktopAppStatus {
    pub state: DesktopAppState,
    /// UXDuck opens in it, rather than in a browser.
    pub chosen: bool,
    /// The version offered for this computer, with its download size and
    /// its size on disk, from the release manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub unpacked_size: Option<u64>,
    /// While downloading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub received: Option<u64>,
    /// The version on this computer, when one is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// `desktopApp/status`: `refresh` asks the release channel again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DesktopAppStatusParams {
    #[serde(default)]
    pub refresh: bool,
}

/// `desktopApp/choose`: open UXDuck in the desktop app, or in a browser.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DesktopAppChooseParams {
    pub on: bool,
}

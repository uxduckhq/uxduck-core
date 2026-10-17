//! Where this computer is (ADR-0030, S137): the tray asks the system as
//! UXDuck and reports its fixes; the daemon keeps the latest in memory and
//! answers `location/status`, which the desktop window and the agent read.
//! Never journaled or written to disk.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Where location stands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum LocationState {
    /// Nobody has turned it on, or it was turned off.
    #[default]
    Off,
    /// Turned on: the system is asking the person, or looking.
    Asking,
    /// A fix arrives as the computer moves.
    On,
    /// The person or the system refused location to UXDuck.
    Denied,
    /// Allowed, but no position could be had (no Wi-Fi, no location
    /// service, a timeout).
    Unavailable,
    /// This UXDuck cannot ask: no menu-bar app, no desktop session, or a
    /// system without a location service.
    Unsupported,
}

/// One position from the system.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LocationFix {
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    /// The system's horizontal accuracy, when it gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accuracy_m: Option<f64>,
    /// When UXDuck received it, Unix milliseconds.
    #[schemars(schema_with = "crate::safe::integer")]
    pub at_ms: u64,
}

/// `location/status`, `location/start` and `location/stop` answer this and
/// `location/changed` pushes it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LocationStatus {
    pub state: LocationState,
    /// The latest fix while location is on; kept, greyed, while it is
    /// unavailable; none once it is off or refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<LocationFix>,
    /// What to tell the person when it is refused, unavailable or
    /// unsupported, in the words the app shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `tray/location`: what the system said, from the tray.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TrayLocationParams {
    /// `asking`, `on`, `denied`, `unavailable` or `unsupported`; `off` is
    /// the daemon's to say.
    pub state: LocationState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latitude_deg: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub longitude_deg: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accuracy_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `tray/locationWanted`: whether the tray should be asking the system.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TrayLocationWanted {
    pub on: bool,
}

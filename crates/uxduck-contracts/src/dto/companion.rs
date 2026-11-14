//! A vehicle's companion computers on the wire (S296, ADR-0049 O7, O8).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What a computer runs once set up: `uxduck forward --onboard` on the one
/// wired to the flight controller, `uxduck companion` on one that only
/// reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CompanionRole {
    Forward,
    #[default]
    Report,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CheckState {
    Checking,
    Reached,
    Failed,
}

/// What the last `computer/check` found, or why it could not.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CompanionCheck {
    pub state: CheckState,
    /// When it started or finished.
    #[schemars(schema_with = "crate::safe::integer")]
    pub at_us: u64,
    /// Its system, such as `Debian GNU/Linux 12 (bookworm)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// `uname -m`: `aarch64`, `armv7l`, `x86_64`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    /// `uxduck --version`; absent when it has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uxduck: Option<String>,
    /// Its host key's fingerprint, pinned on the first check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_key: Option<String>,
    /// Why it failed, for people.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<CompanionNetwork>,
}

/// A companion computer as the person added it. Its private key, if one
/// was given, stays in the daemon's credential store and is never sent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CompanionComputer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connectivity: Option<VehicleConnectivity>,
    pub id: String,
    pub name: String,
    /// A host name, an address, or a `Host` in the person's `~/.ssh/config`.
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    pub role: CompanionRole,
    /// The onboard computer id it reports as, 191 to 194.
    pub component_id: u8,
    /// A private key is kept for it; absent, the person's SSH config and
    /// agent sign in.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub key: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<CompanionCheck>,
}

/// `computer/add`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComputerAddParams {
    /// The vehicle, by id or an earlier alias.
    pub vehicle_id: String,
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default)]
    pub role: CompanionRole,
    /// Absent: its host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A private key in OpenSSH form, only when the person's SSH config and
    /// agent cannot sign in. Kept in the credential store, never returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key: Option<String>,
}

/// `computer/check`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComputerCheckParams {
    pub vehicle_id: String,
    pub computer_id: String,
    /// Its password, used for this check alone to sign in and add this
    /// computer's own UXDuck key to its `authorized_keys`, so later checks
    /// need none. Never kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

/// `computer/remove`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComputerParams {
    pub vehicle_id: String,
    pub computer_id: String,
}

/// A saved network, without its credentials.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WifiNetwork {
    pub id: String,
    pub ssid: String,
    pub active: bool,
    /// Profiles created by UXDuck can be updated here.
    pub managed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CompanionNetwork {
    pub wifi: Vec<WifiNetwork>,
    pub connectivity: String,
    pub checks_enabled: bool,
    #[serde(default)]
    pub checks_available: bool,
    pub check_uri: String,
    #[serde(default)]
    pub connections: Vec<NetworkConnection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preference: Option<NetworkPreference>,
    #[serde(default)]
    pub default_devices: Vec<String>,
    /// False for ambiguous, multipath or unsupported default routing.
    #[serde(default)]
    pub routes_known: bool,
    #[serde(default)]
    pub transport: InternetTransport,
    /// Daemon observation time, never refreshed by a failed check.
    #[serde(default)]
    #[schemars(schema_with = "crate::safe::integer")]
    pub sampled_at_us: u64,
}

/// Read networking, save Wi-Fi for automatic connection when available, or
/// configure internet health checks. At most one change per request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComputerNetworkParams {
    /// A periodic read leaves editable fields and the last good status alone.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub background: bool,
    pub vehicle_id: String,
    pub computer_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wifi: Option<WifiProfileParams>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connectivity: Option<ConnectivityParams>,
    /// Apply Wi-Fi preference and cellular backup with automatic rollback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preference: Option<NetworkPreference>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum WifiSecurity {
    Open,
    WpaPsk,
    Sae,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WifiProfileParams {
    pub ssid: String,
    pub security: WifiSecurity,
    /// Used once on the companion. Never stored in UXDuck or returned.
    pub password: String,
    pub priority: i32,
    pub hidden: bool,
    pub metered: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConnectivityParams {
    /// An HTTP(S) endpoint returning 204 or an empty response.
    pub uri: String,
    /// Seconds between checks. Zero disables them, otherwise 30 to 3600.
    pub interval: u32,
}

impl std::fmt::Debug for WifiProfileParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WifiProfileParams")
            .field("ssid", &self.ssid)
            .field("security", &self.security)
            .field("password", &"[redacted]")
            .finish_non_exhaustive()
    }
}

/// Internet video policy for cameras assigned to this vehicle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CellularVideoPolicy {
    #[default]
    Allow,
    LowData,
    TelemetryOnly,
}

/// Its presence selects this companion as the vehicle's internet gateway.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VehicleConnectivity {
    pub video: CellularVideoPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPreference {
    pub wifi_id: String,
    pub cellular_id: String,
}

/// A saved connection and its current device, without credentials.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NetworkConnection {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub device: Option<String>,
    pub ipv4: String,
    pub ipv6: String,
}

/// Select a vehicle's internet gateway and save its video policy locally.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComputerConnectivityParams {
    pub vehicle_id: String,
    pub computer_id: String,
    pub video: CellularVideoPolicy,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum InternetTransport {
    Wifi,
    Cellular,
    #[default]
    Unknown,
}

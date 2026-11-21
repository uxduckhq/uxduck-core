//! A vehicle's companion computers (S296, ADR-0049 O7, O8): the computers
//! on it that the person added by host name to set up over SSH. What they
//! report arrives as telemetry (S295); this is what UXDuck keeps to reach
//! them. Kept on this computer with the vehicle, never in the account's
//! library: host names and keys are this computer's.

use serde::{Deserialize, Serialize};

/// The onboard computer ids MAVLink keeps: `MAV_COMP_ID_ONBOARD_COMPUTER`
/// to `MAV_COMP_ID_ONBOARD_COMPUTER4`, 191 to 194. Each of a vehicle's
/// computers reports under one of them (ADR-0049), so a vehicle has four
/// at most. The number is MAVLink's and not UXDuck's: a fifth computer
/// would have no id to report under, and two under one id would read as
/// one computer.
pub const COMPONENT_IDS: std::ops::RangeInclusive<u8> = 191..=194;

/// What it runs once set up: `uxduck forward --onboard` on the one wired to
/// the flight controller, `uxduck companion` on one that only reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompanionRole {
    Forward,
    #[default]
    Report,
}

/// The last check's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckState {
    Checking,
    Reached,
    Failed,
}

/// What a failed check ran into (FB-268), so the app offers what helps:
/// a check with the password only when signing in is what failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckProblem {
    /// The computer refused every key offered: a check with its password
    /// adds UXDuck's own.
    SignIn,
    /// The computer refused the password: a wrong password, or no such user.
    Password,
    /// The computer takes no passwords over SSH.
    PasswordOff,
    /// UXDuck's own key cannot be used. A check with the password replaces it.
    Key,
    /// The private key the person gave for this computer cannot be used.
    GivenKey,
    /// Its host key is not the one pinned.
    HostKey,
    /// Not found, not answering, or not running SSH.
    Unreachable,
    /// Anything else, said in the problem's own words.
    Other,
}

/// What the last check found, or why it could not.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompanionCheck {
    pub state: CheckState,
    /// When it started or finished.
    pub at_us: u64,
    /// Its system, such as `Debian GNU/Linux 12 (bookworm)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// Its architecture, as `uname -m` says: `aarch64`, `armv7l`, `x86_64`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    /// The UXDuck it has, as `uxduck --version` says; absent when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uxduck: Option<String>,
    /// Its host key's fingerprint, pinned on the first check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_key: Option<String>,
    /// Why it failed, for people.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    /// What kind of problem it was, when it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem_kind: Option<CheckProblem>,
    /// What `ssh` or `ssh-keygen` said when it failed, without their debug
    /// lines, shown under the problem for whoever needs it. Never a key or
    /// a password, which neither prints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Its clock as the check read it (S356); absent when it could not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock: Option<CompanionClock>,
    /// The onboard computer id its running `uxduck companion` or
    /// `uxduck forward --onboard` was given with `--computer-id` (S372),
    /// kept from the last check that read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reports_as: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<CompanionNetwork>,
    /// What its `MediaMTX` serves, as last read (ADR-0060).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cameras: Option<super::companion_cameras::CompanionCameras>,
}

/// A computer's clock as a check read it (S356): its time against this
/// computer's, timed around the SSH command, and whether it keeps time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompanionClock {
    /// Its clock less this computer's, in milliseconds: positive when it
    /// is ahead.
    pub offset_ms: i64,
    /// How far the true offset may be from `offset_ms`: half the time the
    /// SSH command took, signing in included.
    pub within_ms: u64,
    /// Whether its kernel says the clock is synchronised (`timedatectl`'s
    /// `NTPSynchronized`); absent where `timedatectl` is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synchronised: Option<bool>,
    /// Whether network time is turned on (`timedatectl`'s `NTP`); absent
    /// where `timedatectl` is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ntp: Option<bool>,
    /// The time service that runs, such as `systemd-timesyncd` or
    /// `chrony`; absent when none does or it cannot be told.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompanionComputer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connectivity: Option<VehicleConnectivity>,
    pub id: String,
    pub name: String,
    /// A host name, an address, or a `Host` in the person's `~/.ssh/config`.
    pub host: String,
    /// The user to sign in as; absent, SSH's own choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// The SSH port; absent, SSH's own choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default)]
    pub role: CompanionRole,
    /// The onboard computer id it reports as, 191 to 194.
    pub component_id: u8,
    /// A private key is kept for it in the credential store; absent, the
    /// person's SSH config and agent sign in.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub key: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<CompanionCheck>,
}

impl CompanionComputer {
    /// The credential holding its private key.
    #[must_use]
    pub fn key_name(&self) -> String {
        format!("companion-key-{}", self.id)
    }
}

/// Why a host, user or name cannot be used. Each is passed to `ssh` as an
/// argument, so nothing may start with `-` or hold spaces or `@` where
/// SSH would read them as something else.
///
/// # Errors
/// What is wrong, for people.
pub fn check_address(host: &str, user: Option<&str>) -> Result<(), String> {
    let plain = |s: &str| {
        !s.is_empty()
            && !s.starts_with('-')
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-:[]%".contains(c))
    };
    if !plain(host) {
        return Err(format!(
            "\"{host}\" is not a host name: letters, digits, dots and dashes, or an address"
        ));
    }
    if let Some(user) = user
        && (!plain(user) || user.contains(':'))
    {
        return Err(format!("\"{user}\" is not a user name"));
    }
    Ok(())
}

/// The first onboard computer id none of `computers` has.
#[must_use]
pub fn free_component_id(computers: &[CompanionComputer]) -> Option<u8> {
    COMPONENT_IDS
        .into_iter()
        .find(|id| computers.iter().all(|c| c.component_id != *id))
}

/// The computer `computer_id` takes the id its check found its UXDuck
/// sends as (S372), because its readings arrive under that id whatever
/// order the computers were added in. Another computer given that id when
/// it was added moves to the first free one. One whose own check found it
/// sends as that id too keeps it, as both do on the wire, and their cards
/// say so.
pub fn take_reported_id(computers: &mut [CompanionComputer], computer_id: &str) {
    let reported = |c: &CompanionComputer| c.check.as_ref().and_then(|check| check.reports_as);
    let Some(at) = computers.iter().position(|c| c.id == computer_id) else {
        return;
    };
    let Some(id) = reported(&computers[at]) else {
        return;
    };
    if computers[at].component_id == id {
        return;
    }
    computers[at].component_id = id;
    for other in 0..computers.len() {
        if other == at
            || computers[other].component_id != id
            || reported(&computers[other]) == Some(id)
        {
            continue;
        }
        if let Some(free) = free_component_id(computers) {
            computers[other].component_id = free;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_hosts_and_users_reach_ssh() {
        assert!(check_address("pi-vision.local", Some("pi")).is_ok());
        assert!(check_address("192.168.1.20", None).is_ok());
        assert!(check_address("fe80::1%en0", None).is_ok());
        assert!(check_address("-oProxyCommand=x", None).is_err());
        assert!(check_address("a b", None).is_err());
        assert!(check_address("pi@host", None).is_err());
        assert!(check_address("host", Some("-l")).is_err());
        assert!(check_address("", None).is_err());
    }
}

/// A saved network, without its credentials.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WifiNetwork {
    pub id: String,
    pub ssid: String,
    pub active: bool,
    /// Profiles created by UXDuck can be updated here.
    pub managed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Daemon observation time, never refreshed by a failed check.
    #[serde(default)]
    pub sampled_at_us: u64,
}

/// Internet video policy for cameras assigned to this vehicle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CellularVideoPolicy {
    #[default]
    Allow,
    LowData,
    TelemetryOnly,
}

/// Its presence selects this companion as the vehicle's internet gateway.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VehicleConnectivity {
    pub video: CellularVideoPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPreference {
    pub wifi_id: String,
    pub cellular_id: String,
}

/// A saved connection and its current device, without credentials.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkConnection {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub device: Option<String>,
    pub ipv4: String,
    pub ipv6: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InternetTransport {
    Wifi,
    Cellular,
    Unknown,
}

impl CompanionNetwork {
    /// Conservatively classify all effective default routes, including IPv6.
    #[must_use]
    pub fn transport(&self) -> InternetTransport {
        if !self.routes_known
            || !self.checks_enabled
            || !self.checks_available
            || self.default_devices.is_empty()
        {
            return InternetTransport::Unknown;
        }
        let Some(preference) = &self.preference else {
            return InternetTransport::Unknown;
        };
        let device = |id: &str| {
            self.connections
                .iter()
                .find(|c| c.id == id)
                .and_then(|c| c.device.as_deref())
        };
        let wifi = self
            .connections
            .iter()
            .find(|c| c.kind == "802-11-wireless" && c.device.is_some())
            .and_then(|c| c.device.as_deref());
        let cellular = device(&preference.cellular_id);
        if self
            .default_devices
            .iter()
            .any(|d| Some(d.as_str()) != wifi && Some(d.as_str()) != cellular)
        {
            return InternetTransport::Unknown;
        }
        if self
            .default_devices
            .iter()
            .any(|d| Some(d.as_str()) == cellular)
        {
            if self
                .connections
                .iter()
                .any(|c| c.id == preference.cellular_id && (c.ipv4 == "full" || c.ipv6 == "full"))
            {
                InternetTransport::Cellular
            } else {
                InternetTransport::Unknown
            }
        } else if self
            .connections
            .iter()
            .any(|c| c.device.as_deref() == wifi && (c.ipv4 == "full" || c.ipv6 == "full"))
        {
            InternetTransport::Wifi
        } else {
            InternetTransport::Unknown
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test fixture")]
mod network_tests {
    use super::*;
    fn network() -> CompanionNetwork {
        CompanionNetwork {
            wifi: vec![],
            connectivity: "full".into(),
            checks_enabled: true,
            checks_available: true,
            check_uri: "https://check.test/204".into(),
            preference: Some(NetworkPreference {
                wifi_id: "wifi".into(),
                cellular_id: "cell".into(),
            }),
            connections: vec![
                NetworkConnection {
                    id: "wifi".into(),
                    name: "Office".into(),
                    kind: "802-11-wireless".into(),
                    device: Some("wlan0".into()),
                    ipv4: "full".into(),
                    ipv6: "none".into(),
                },
                NetworkConnection {
                    id: "cell".into(),
                    name: "Modem".into(),
                    kind: "802-3-ethernet".into(),
                    device: Some("usb0".into()),
                    ipv4: "full".into(),
                    ipv6: "full".into(),
                },
            ],
            default_devices: vec!["wlan0".into()],
            routes_known: true,
            sampled_at_us: 1,
        }
    }
    #[test]
    fn transport_requires_healthy_known_routes_and_counts_cellular_ipv6() {
        let mut n = network();
        assert_eq!(n.transport(), InternetTransport::Wifi);
        n.default_devices.push("usb0".into());
        assert_eq!(n.transport(), InternetTransport::Cellular);
        n.default_devices.push("tun0".into());
        assert_eq!(n.transport(), InternetTransport::Unknown);
        n = network();
        n.connections[0].ipv4 = "limited".into();
        assert_eq!(n.transport(), InternetTransport::Unknown);
        n = network();
        n.checks_available = false;
        assert_eq!(n.transport(), InternetTransport::Unknown);
        n = network();
        n.routes_known = false;
        assert_eq!(n.transport(), InternetTransport::Unknown);
        n = network();
        n.preference = None;
        assert_eq!(n.transport(), InternetTransport::Unknown);
    }
    #[test]
    fn old_snapshots_do_not_claim_verified_internet() {
        let n: CompanionNetwork = serde_json::from_value(serde_json::json!({
            "wifi":[], "connectivity":"full", "checksEnabled":true, "checkUri":"https://check.test/204"
        })).unwrap();
        assert_eq!(n.transport(), InternetTransport::Unknown);
        assert_eq!(n.sampled_at_us, 0);
    }
}

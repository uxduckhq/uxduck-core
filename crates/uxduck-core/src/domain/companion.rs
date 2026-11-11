//! A vehicle's companion computers (S296, ADR-0049 O7, O8): the computers
//! on it that the person added by host name to set up over SSH. What they
//! report arrives as telemetry (S295); this is what UXDuck keeps to reach
//! them. Kept on this computer with the vehicle, never in the account's
//! library: host names and keys are this computer's.

use serde::{Deserialize, Serialize};

/// The onboard computer ids MAVLink keeps (191 to 194).
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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompanionComputer {
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

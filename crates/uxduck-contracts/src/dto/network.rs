//! Phones and tablets on this computer's network (S97, ADR-0001 D4): the
//! addresses UXDuck could listen on and where it listens now.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One of this computer's IPv4 addresses that a device on the same network
/// could reach.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NetworkAddress {
    /// For example `192.168.1.20`.
    pub address: String,
    /// The network interface, for example `en0`.
    pub interface: String,
    /// Readable system label, for example Wi-Fi, Ethernet or Tailscale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The one the computer's usual route goes out of: the best guess.
    #[serde(default)]
    pub usual: bool,
}

/// Result of `network/status`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NetworkStatus {
    /// The addresses there are now, the usual one first.
    pub addresses: Vec<NetworkAddress>,
    /// While listening: what a phone on the network opens, such as
    /// `http://192.168.1.20:45000`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Every active listener, in the order selected. `url` is the first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub urls: Vec<String>,
    /// Turned on but not listening, and why, in words for the person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    /// The client ids of paired phones and tablets connected now.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connected: Vec<String>,
}

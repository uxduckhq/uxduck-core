//! Readable interface labels, separate from the identifiers used for binding.
//! Metadata is optional: unknown interfaces keep their system identifier in
//! the UI. Never infer a VPN provider from its IP range or tunnel number.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use uxduck_contracts::dto::network::NetworkAddress;

#[derive(Default)]
pub(super) struct Names {
    labels: BTreeMap<String, String>,
    interfaces: Vec<String>,
    checked: Option<Instant>,
}

impl Names {
    pub async fn apply(&mut self, addresses: &mut [NetworkAddress]) {
        let interfaces: Vec<String> = addresses
            .iter()
            .map(|a| format!("{}/{}", a.interface, a.address))
            .collect();
        if self.interfaces != interfaces
            || self
                .checked
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(30))
        {
            self.labels = read_labels().await;
            self.interfaces = interfaces;
            self.checked = Some(Instant::now());
        }
        for address in addresses {
            address.label = Some(
                self.labels
                    .get(&address.interface)
                    .cloned()
                    .unwrap_or_else(|| fallback(&address.interface)),
            );
        }
    }
}

#[cfg(target_os = "macos")]
async fn read_labels() -> BTreeMap<String, String> {
    let mut command = tokio::process::Command::new("/sbin/ifconfig");
    command.args(["-v"]).env("LC_ALL", "C").kill_on_drop(true);
    match tokio::time::timeout(Duration::from_secs(1), command.output()).await {
        Ok(Ok(output)) if output.status.success() => {
            from_macos(&String::from_utf8_lossy(&output.stdout))
        }
        Ok(Ok(output)) => {
            tracing::warn!(status = ?output.status, "could not read interface labels; using interface types");
            BTreeMap::new()
        }
        Ok(Err(error)) => {
            tracing::warn!(%error, "could not read interface labels; using interface types");
            BTreeMap::new()
        }
        Err(_) => {
            tracing::warn!("reading interface labels timed out; using interface types");
            BTreeMap::new()
        }
    }
}

#[cfg(not(target_os = "macos"))]
#[expect(clippy::unused_async, reason = "the same signature as macOS's reader")]
async fn read_labels() -> BTreeMap<String, String> {
    BTreeMap::new()
}

fn fallback(interface: &str) -> String {
    #[cfg(target_os = "linux")]
    {
        let path = std::path::Path::new("/sys/class/net").join(interface);
        if path.join("wireless").exists() {
            return "Wi-Fi".into();
        }
        if let Ok(alias) = std::fs::read_to_string(path.join("ifalias")) {
            let alias = alias.trim();
            if !alias.is_empty() {
                return alias.to_owned();
            }
        }
        if path.join("bridge").exists() {
            return "Network bridge".into();
        }
        if path.join("device").exists()
            && std::fs::read_to_string(path.join("type")).is_ok_and(|t| t.trim() == "1")
        {
            return "Ethernet".into();
        }
    }
    #[cfg(windows)]
    {
        // if-addrs already uses GetAdaptersAddresses' FriendlyName on Windows.
        interface.to_owned()
    }
    #[cfg(not(windows))]
    match interface {
        name if name.starts_with("tailscale") => "Tailscale",
        name if name.starts_with("utun")
            || name.starts_with("tun")
            || name.starts_with("tap")
            || name.starts_with("wg") =>
        {
            "Network tunnel"
        }
        name if name.starts_with("bridge") || name.starts_with("br-") => "Network bridge",
        _ => "Network interface",
    }
    .into()
}

/// `ifconfig -v` reports the hardware type and the VPN's own agent label.
#[cfg(any(target_os = "macos", test))]
fn from_macos(text: &str) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    let mut interface = None;
    for line in text.lines() {
        if !line.starts_with(char::is_whitespace) {
            interface = line.split_once(": flags=").map(|(name, _)| name.to_owned());
        }
        let Some(name) = &interface else {
            continue;
        };
        let line = line.trim();
        if let Some(kind) = line.strip_prefix("type: ") {
            let label = match kind {
                "Wi-Fi" | "IEEE80211" => "Wi-Fi",
                "Ethernet" => "Ethernet",
                _ => continue,
            };
            labels
                .entry(name.clone())
                .or_insert_with(|| label.to_owned());
        } else if line.starts_with("agent domain:NetworkExtension type:VPN ") {
            if let Some(provider) = line
                .split_once("desc:\"VPN: ")
                .and_then(|(_, rest)| rest.split_once('"').map(|(label, _)| label.trim()))
                .filter(|label| !label.is_empty())
            {
                labels.insert(name.clone(), provider.to_owned());
            }
        } else if line == "desc: com.apple.NetworkSharing" {
            labels.insert(name.clone(), "Internet Sharing".into());
        }
    }
    labels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_and_vpn_labels_follow_metadata_not_interface_numbers() {
        let labels = from_macos(
            "en0: flags=1\n\ttype: Ethernet\nen7: flags=1\n\ttype: Wi-Fi\nutun4: flags=1\n\tagent domain:NetworkExtension type:VPN flags:0xf desc:\"VPN: Tailscale\"\nutun9: flags=1\n\tagent domain:NetworkExtension type:VPN flags:0xf desc:\"VPN: Another VPN\"\nutun8: flags=1\n\tinet 100.64.1.2\nbridge100: flags=1\n\tdesc: com.apple.NetworkSharing\n",
        );
        assert_eq!(labels.get("en0").map(String::as_str), Some("Ethernet"));
        assert_eq!(labels.get("en7").map(String::as_str), Some("Wi-Fi"));
        assert_eq!(labels.get("utun4").map(String::as_str), Some("Tailscale"));
        assert_eq!(labels.get("utun9").map(String::as_str), Some("Another VPN"));
        assert_eq!(
            labels.get("bridge100").map(String::as_str),
            Some("Internet Sharing")
        );
        assert!(!labels.contains_key("utun8"));
    }

    #[test]
    fn unknown_metadata_does_not_leak_the_previous_interfaces_label() {
        let labels = from_macos(
            "en7: flags=1\n\ttype: Wi-Fi\nunknown: flags=1\n\ttype: Unknown\n\tagent domain:Skywalk type:NetIf flags:0xf desc:\"VPN: Not a VPN\"\n",
        );
        assert!(!labels.contains_key("unknown"));
        assert!(from_macos("unrecognised output").is_empty());
    }
}

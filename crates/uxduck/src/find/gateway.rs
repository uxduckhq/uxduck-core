//! The default gateway's address, as the system's routing table has it.
//! Only read, never set; the Wi-Fi network's name is never read (F4).

use std::net::Ipv4Addr;

#[must_use]
pub fn default_gateway() -> Option<Ipv4Addr> {
    if cfg!(target_os = "linux") {
        from_proc_route(&std::fs::read_to_string("/proc/net/route").ok()?)
    } else if cfg!(target_os = "macos") {
        let out = std::process::Command::new("route")
            .args(["-n", "get", "default"])
            .output()
            .ok()?;
        from_bsd_route(&String::from_utf8_lossy(&out.stdout))
    } else if cfg!(windows) {
        let out = std::process::Command::new("route")
            .args(["print", "0.0.0.0"])
            .output()
            .ok()?;
        from_windows_route(&String::from_utf8_lossy(&out.stdout))
    } else {
        None
    }
}

/// Linux: the row whose destination is 0, its gateway in little-endian hex.
fn from_proc_route(table: &str) -> Option<Ipv4Addr> {
    table.lines().skip(1).find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.get(1) != Some(&"00000000") {
            return None;
        }
        let raw = u32::from_str_radix(fields.get(2)?, 16).ok()?;
        (raw != 0).then(|| Ipv4Addr::from(raw.swap_bytes()))
    })
}

/// macOS: `gateway: 192.168.4.1` in `route -n get default`.
fn from_bsd_route(text: &str) -> Option<Ipv4Addr> {
    text.lines()
        .find_map(|l| l.trim().strip_prefix("gateway:"))
        .and_then(|g| g.trim().parse().ok())
}

/// Windows: the `0.0.0.0  0.0.0.0  <gateway>` row of `route print`.
fn from_windows_route(text: &str) -> Option<Ipv4Addr> {
    text.lines().find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        (fields.first() == Some(&"0.0.0.0") && fields.get(1) == Some(&"0.0.0.0"))
            .then(|| fields.get(2)?.parse().ok())
            .flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gateway_is_read_from_each_platforms_table() {
        let linux = "Iface\tDestination\tGateway \tFlags\n\
                     wlan0\t0000A8C0\t00000000\t0001\n\
                     wlan0\t00000000\t0104A8C0\t0003\n";
        assert_eq!(from_proc_route(linux), Some(Ipv4Addr::new(192, 168, 4, 1)));
        let mac = "   route to: default\ndestination: default\n    gateway: 192.168.1.254\n  interface: en0\n";
        assert_eq!(from_bsd_route(mac), Some(Ipv4Addr::new(192, 168, 1, 254)));
        let windows = "Network Destination        Netmask          Gateway       Interface  Metric\n\
                       0.0.0.0          0.0.0.0      10.0.0.1     10.0.0.23     25\n";
        assert_eq!(
            from_windows_route(windows),
            Some(Ipv4Addr::new(10, 0, 0, 1))
        );
        assert_eq!(from_bsd_route("no route"), None);
    }
}

//! Ports as shared territory (ADR-0004 port ownership): who holds a port,
//! whether a listener could bind, which addresses are ours, and stopping a
//! program of the same user that holds one.

use std::net::{IpAddr, UdpSocket};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Udp,
    /// Listening TCP sockets.
    Tcp,
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Udp => "UDP",
            Self::Tcp => "TCP",
        })
    }
}

/// A process holding a port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owner {
    pub pid: u32,
    pub name: String,
    /// The owning user where the platform says (never on Windows).
    pub uid: Option<u32>,
}

impl std::fmt::Display for Owner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (pid {})", self.name, self.pid)
    }
}

/// "UDP port 14550 is in use by `MissionPlanner` (pid 812)", or "by another
/// program" when the owner cannot be read.
#[must_use]
pub fn in_use_message(protocol: Protocol, port: u16) -> String {
    match owner(protocol, port) {
        Some(owner) => format!("{protocol} port {port} is in use by {owner}"),
        None => format!("{protocol} port {port} is in use by another program"),
    }
}

/// Whether a UDP listener could bind here now, without address reuse.
///
/// # Errors
/// A message for people: who holds the port, or why the address is unusable.
pub fn check_udp_bind(host: &str, port: u16) -> Result<(), String> {
    let ip: IpAddr = host
        .parse()
        .map_err(|_| format!("{host} is not an IP address"))?;
    match UdpSocket::bind((ip, port)) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            Err(in_use_message(Protocol::Udp, port))
        }
        Err(error) => Err(format!("cannot listen on {host}:{port}: {error}")),
    }
}

/// Whether an address belongs to this machine: loopback, unspecified, or one
/// of its interfaces (it can be bound).
#[must_use]
pub fn is_local_address(host: &str) -> bool {
    if host == "localhost" {
        return true;
    }
    let Ok(ip) = host.parse::<IpAddr>() else {
        return false;
    };
    ip.is_loopback() || ip.is_unspecified() || UdpSocket::bind((ip, 0)).is_ok()
}

/// The process holding a port, where the platform lets us read it.
#[must_use]
pub fn owner(protocol: Protocol, port: u16) -> Option<Owner> {
    platform::owner(protocol, port)
}

/// The executable of a local process, for a client's identity (ADR-0005).
#[cfg(unix)]
#[must_use]
pub fn executable_of(pid: u32) -> Option<String> {
    if cfg!(target_os = "linux") {
        return std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .map(|p| p.display().to_string());
    }
    if cfg!(target_os = "macos") {
        let output = std::process::Command::new("ps")
            .args(["-o", "comm=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        return (!path.is_empty()).then_some(path);
    }
    None
}

/// Why a port cannot be freed, or what was stopped.
#[derive(Debug, PartialEq, Eq)]
pub enum Free {
    /// A system process or another user's: never offered.
    NotYours(Owner),
    Stopped(Owner),
    Failed(Owner, String),
}

/// Whether we would offer to stop this owner: the same user, not a system
/// process, and not ourselves.
#[must_use]
pub fn may_stop(owner: &Owner) -> bool {
    owner.pid > 1
        && owner.pid != std::process::id()
        && owner
            .uid
            .is_some_and(|uid| uid != 0 && Some(uid) == platform::current_uid())
}

/// Stops the process holding a port, if [`may_stop`] allows it. The caller
/// asks for confirmation first, naming the process.
#[must_use]
pub fn free(owner: Owner) -> Free {
    if !may_stop(&owner) {
        return Free::NotYours(owner);
    }
    match platform::terminate(owner.pid) {
        Ok(()) => Free::Stopped(owner),
        Err(error) => Free::Failed(owner, error),
    }
}

/// Without `unsafe` (the workspace forbids it), through the standard tools.
#[cfg(unix)]
mod unix {
    pub fn current_uid() -> Option<u32> {
        let output = std::process::Command::new("id").arg("-u").output().ok()?;
        String::from_utf8_lossy(&output.stdout).trim().parse().ok()
    }

    pub fn terminate(pid: u32) -> Result<(), String> {
        let status = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .map_err(|e| e.to_string())?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("kill exited with {status}"))
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{Owner, Protocol};

    pub fn current_uid() -> Option<u32> {
        super::unix::current_uid()
    }

    pub fn terminate(pid: u32) -> Result<(), String> {
        super::unix::terminate(pid)
    }

    /// Socket inodes bound to the port, from /proc/net/{udp,tcp}{,6}.
    fn inodes(protocol: Protocol, port: u16) -> Vec<String> {
        let (files, listen_only) = match protocol {
            Protocol::Udp => (["/proc/net/udp", "/proc/net/udp6"], false),
            Protocol::Tcp => (["/proc/net/tcp", "/proc/net/tcp6"], true),
        };
        let mut found = Vec::new();
        for file in files {
            let Ok(text) = std::fs::read_to_string(file) else {
                continue;
            };
            for line in text.lines().skip(1) {
                let fields: Vec<&str> = line.split_whitespace().collect();
                let (Some(local), Some(state), Some(inode)) =
                    (fields.get(1), fields.get(3), fields.get(9))
                else {
                    continue;
                };
                let local_port = local
                    .rsplit(':')
                    .next()
                    .and_then(|hex| u16::from_str_radix(hex, 16).ok());
                // 0A is LISTEN for TCP.
                if local_port == Some(port) && (!listen_only || *state == "0A") && *inode != "0" {
                    found.push((*inode).to_owned());
                }
            }
        }
        found
    }

    pub fn owner(protocol: Protocol, port: u16) -> Option<Owner> {
        let inodes = inodes(protocol, port);
        if inodes.is_empty() {
            return None;
        }
        let targets: Vec<String> = inodes.iter().map(|i| format!("socket:[{i}]")).collect();
        for entry in std::fs::read_dir("/proc").ok()?.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
                continue;
            };
            let holds = fds.flatten().any(|fd| {
                std::fs::read_link(fd.path())
                    .is_ok_and(|target| targets.iter().any(|t| target.to_string_lossy() == *t))
            });
            if holds {
                let name = std::fs::read_to_string(entry.path().join("comm"))
                    .map_or_else(|_| "unknown".into(), |n| n.trim().to_owned());
                let uid = std::fs::read_to_string(entry.path().join("status"))
                    .ok()
                    .and_then(|status| {
                        status
                            .lines()
                            .find_map(|l| l.strip_prefix("Uid:"))
                            .and_then(|rest| rest.split_whitespace().next()?.parse().ok())
                    });
                return Some(Owner { pid, name, uid });
            }
        }
        None
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{Owner, Protocol};

    pub fn current_uid() -> Option<u32> {
        super::unix::current_uid()
    }

    pub fn terminate(pid: u32) -> Result<(), String> {
        super::unix::terminate(pid)
    }

    /// `lsof` ships with macOS. `-F pcu` prints the pid, command and uid.
    pub fn owner(protocol: Protocol, port: u16) -> Option<Owner> {
        let spec = match protocol {
            Protocol::Udp => format!("-iUDP:{port}"),
            Protocol::Tcp => format!("-iTCP:{port}"),
        };
        let mut command = std::process::Command::new("lsof");
        command.args(["-nP", &spec, "-Fpcu"]);
        if protocol == Protocol::Tcp {
            command.arg("-sTCP:LISTEN");
        }
        let output = command.output().ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        let (mut pid, mut name, mut uid) = (None, None, None);
        for line in text.lines() {
            match line.split_at_checked(1) {
                Some(("p", rest)) => pid = rest.parse().ok(),
                Some(("c", rest)) => name = Some(rest.to_owned()),
                Some(("u", rest)) => uid = rest.parse().ok(),
                _ => {}
            }
            if pid.is_some() && name.is_some() && uid.is_some() {
                break;
            }
        }
        Some(Owner {
            pid: pid?,
            name: name.unwrap_or_else(|| "unknown".into()),
            uid,
        })
    }
}

#[cfg(windows)]
mod platform {
    use super::{Owner, Protocol};

    /// Windows does not give a comparable uid without elevation, so no port
    /// is ever offered for freeing there (ADR-0004).
    pub fn current_uid() -> Option<u32> {
        None
    }

    pub fn terminate(_: u32) -> Result<(), String> {
        Err("not offered on Windows".into())
    }

    pub fn owner(protocol: Protocol, port: u16) -> Option<Owner> {
        let proto = match protocol {
            Protocol::Udp => "UDP",
            Protocol::Tcp => "TCP",
        };
        let output = std::process::Command::new("netstat")
            .args(["-ano", "-p", proto])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        let pid: u32 = text.lines().find_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let local = fields.get(1)?;
            let listening = protocol == Protocol::Udp || fields.get(3) == Some(&"LISTENING");
            (listening && local.rsplit(':').next()?.parse::<u16>().ok()? == port)
                .then(|| fields.last()?.parse().ok())
                .flatten()
        })?;
        let tasks = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .ok()?;
        let name = String::from_utf8_lossy(&tasks.stdout)
            .split(',')
            .next()
            .map(|n| n.trim_matches('"').to_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "unknown".into());
        Some(Owner {
            pid,
            name,
            uid: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_busy_udp_port_names_its_owner_and_is_ours_to_free() {
        // Let the OS choose an unused port for the successful-bind branch.
        // A released ephemeral port can immediately be claimed by another test.
        assert!(check_udp_bind("127.0.0.1", 0).is_ok());
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        let message = check_udp_bind("127.0.0.1", port).unwrap_err();
        assert!(
            message.starts_with(&format!("UDP port {port} is in use by")),
            "{message}"
        );
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            let found = owner(Protocol::Udp, port).expect("an owner");
            assert_eq!(found.pid, std::process::id());
            assert!(message.contains(&format!("(pid {})", std::process::id())));
            assert!(!may_stop(&found), "never ourselves");
        }
    }

    #[test]
    fn our_own_addresses_are_local() {
        assert!(is_local_address("127.0.0.1"));
        assert!(is_local_address("0.0.0.0"));
        assert!(is_local_address("localhost"));
        assert!(!is_local_address("192.0.2.1"), "TEST-NET-1 is never ours");
        assert!(!is_local_address("not an address"));
    }
}

/// Read-only local port inspection; never frees the port or probes a host.
pub fn inspect(
    params: &uxduck_contracts::dto::diagnostics::SocketInspectParams,
) -> Result<
    uxduck_contracts::dto::diagnostics::SocketInspectResult,
    uxduck_contracts::errors::ApiError,
> {
    use uxduck_contracts::dto::diagnostics::{SocketInspectResult, SocketOwner, SocketProtocol};
    let protocol = if params.protocol == SocketProtocol::Udp {
        Protocol::Udp
    } else {
        Protocol::Tcp
    };
    if params.port == 0 {
        return Err(uxduck_engine::diagnostics::invalid(
            "Choose a socket port from 1 to 65535.",
        ));
    }
    let mut result = SocketInspectResult {
        protocol: params.protocol,
        port: params.port,
        owners: Vec::new(),
        available: true,
        complete: false,
        notes: Vec::new(),
    };
    if cfg!(target_os = "macos") {
        let spec = format!("-i{protocol}:{}", params.port);
        let output = std::process::Command::new("lsof")
            .args(["-nP", "-b", &spec, "-Fpcfn"])
            .output();
        match output {
            Ok(output) => {
                result.owners = sockets_at(&String::from_utf8_lossy(&output.stdout), params.port);
                result.complete = output.status.success()
                    || (output.status.code() == Some(1) && output.stderr.is_empty());
                if !result.complete {
                    result.notes.push("Socket enumeration was incomplete. Permissions or lsof errors may hide endpoints.".into());
                }
                if let Some(host) = &params.host {
                    result.owners.retain(|o| {
                        o.local_address.as_ref().is_some_and(|a| {
                            a.starts_with(&format!("{host}:"))
                                || a.starts_with(&format!("[{host}]:"))
                                || a.starts_with("*:")
                        })
                    });
                }
            }
            Err(error) => {
                result.available = false;
                result
                    .notes
                    .push(format!("Could not run macOS socket inspection: {error}"));
            }
        }
    } else if params.host.is_some() {
        result.available = false;
        result.notes.push("This platform provides port ownership but does not support filtering socket endpoints by host.".into());
    } else {
        if let Some(o) = owner(protocol, params.port) {
            result.owners.push(SocketOwner {
                pid: o.pid,
                name: o.name,
                local_address: None,
                remote_address: None,
            });
        }
        result.notes.push("This platform returns one visible local port owner. Full endpoint enumeration is unavailable. An empty result does not establish that the port is unused.".into());
    }
    if result.owners.len() > 200 {
        result.owners.truncate(200);
        result.complete = false;
        result
            .notes
            .push("Socket results were limited to 200 entries.".into());
    }
    Ok(result)
}

/// lsof's -i matches either endpoint; ownership requires the local port.
fn sockets_at(text: &str, port: u16) -> Vec<uxduck_contracts::dto::diagnostics::SocketOwner> {
    parse_sockets(text)
        .into_iter()
        .filter(|owner| {
            owner.local_address.as_deref().is_some_and(|address| {
                address
                    .rsplit(':')
                    .next()
                    .and_then(|p| p.parse::<u16>().ok())
                    == Some(port)
            })
        })
        .collect()
}

fn parse_sockets(text: &str) -> Vec<uxduck_contracts::dto::diagnostics::SocketOwner> {
    use uxduck_contracts::dto::diagnostics::SocketOwner;
    let (mut pid, mut name) = (None, String::new());
    let mut out = Vec::new();
    for line in text.lines() {
        match line.split_at_checked(1) {
            Some(("p", value)) => {
                pid = value.parse::<u32>().ok();
                name.clear();
            }
            Some(("c", value)) => value.clone_into(&mut name),
            Some(("n", value)) => {
                if let Some(pid) = pid {
                    let (local, remote) = value
                        .split_once("->")
                        .map_or((value, None), |(a, b)| (a, Some(b)));
                    out.push(SocketOwner {
                        pid,
                        name: name.clone(),
                        local_address: Some(local.to_owned()),
                        remote_address: remote.map(str::to_owned),
                    });
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod inspection_tests {
    use super::*;
    #[test]
    fn socket_ownership_preserves_multiple_processes_and_connected_endpoints() {
        let text = "p812\ncQGroundControl\nf12\nn*:14550\np813\ncuxduck\nf13\nn127.0.0.1:50000->127.0.0.1:14550\n";
        let found = parse_sockets(text);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].pid, 812);
        assert_eq!(found[0].name, "QGroundControl");
        assert_eq!(found[0].local_address.as_deref(), Some("*:14550"));
        assert_eq!(found[1].remote_address.as_deref(), Some("127.0.0.1:14550"));
        let owners = sockets_at(text, 14550);
        assert_eq!(owners.len(), 1);
        assert_eq!(
            owners[0].pid, 812,
            "a sender's remote endpoint is not a local port owner"
        );
        assert!(sockets_at(text, 14551).is_empty());
        let ipv6 = sockets_at("p99\ncReceiver\nn[::1]:14550\n", 14550);
        assert_eq!(ipv6[0].local_address.as_deref(), Some("[::1]:14550"));
    }
}

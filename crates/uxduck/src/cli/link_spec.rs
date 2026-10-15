//! Links on the command line, for `uxduck forward`:
//!
//! - `udp:0.0.0.0:15010` listens on UDP;
//! - `udpout:192.168.4.1:14550` sends to a UDP server and hears its vehicles;
//! - `serial:/dev/ttyACM0` or `serial:/dev/ttyACM0@115200` opens a serial device;
//! - `tcp:127.0.0.1:5760` connects to a TCP server;
//! - `tcpin:0.0.0.0:5760` listens for TCP connections;
//! - as `--out`, `udp:192.168.1.20:15010` forwards to that address.

use uxduck_contracts::dto::link::{LinkAddParams, LinkKind, LinkPolicy};

/// The baud rate when a serial spec gives none (ArduPilot's USB default).
pub const DEFAULT_BAUD: u32 = 115_200;

fn host_port(rest: &str, spec: &str) -> Result<(String, u16), String> {
    let (host, port) = rest
        .rsplit_once(':')
        .ok_or_else(|| format!("{spec}: expected HOST:PORT"))?;
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|p| *p > 0)
        .ok_or_else(|| format!("{spec}: {port} is not a port"))?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() {
        return Err(format!("{spec}: no host"));
    }
    Ok((host.to_owned(), port))
}

/// A link vehicles are heard on (`--in`).
///
/// # Errors
/// A message naming what is wrong with the spec.
pub fn input(spec: &str) -> Result<LinkKind, String> {
    let (scheme, rest) = spec
        .split_once(':')
        .ok_or_else(|| format!("{spec}: expected udp:, udpout:, serial:, tcp: or tcpin:"))?;
    match scheme {
        "udp" => {
            let (host, port) = host_port(rest, spec)?;
            Ok(LinkKind::UdpListen { host, port })
        }
        "udpout" => {
            let (host, port) = host_port(rest, spec)?;
            Ok(LinkKind::UdpClient { host, port })
        }
        "tcp" => {
            let (host, port) = host_port(rest, spec)?;
            Ok(LinkKind::TcpClient { host, port })
        }
        "tcpin" => {
            let (host, port) = host_port(rest, spec)?;
            Ok(LinkKind::TcpServer { host, port })
        }
        "serial" => {
            let (device, baud) = match rest.rsplit_once('@') {
                Some((device, baud)) => (
                    device,
                    baud.parse()
                        .ok()
                        .filter(|b| *b > 0)
                        .ok_or_else(|| format!("{spec}: {baud} is not a baud rate"))?,
                ),
                None => (rest, DEFAULT_BAUD),
            };
            if device.is_empty() {
                return Err(format!("{spec}: no device"));
            }
            Ok(LinkKind::Serial {
                device: device.to_owned(),
                baud,
                usb: None,
            })
        }
        other => Err(format!(
            "{spec}: unknown link type {other}; use udp, udpout, serial, tcp or tcpin"
        )),
    }
}

/// A destination (`--out`).
///
/// # Errors
/// A message naming what is wrong with the spec.
pub fn output(spec: &str) -> Result<LinkKind, String> {
    let (scheme, rest) = spec
        .split_once(':')
        .ok_or_else(|| format!("{spec}: expected udp:HOST:PORT"))?;
    if scheme != "udp" {
        return Err(format!("{spec}: forwards are UDP for now (udp:HOST:PORT)"));
    }
    let (host, port) = host_port(rest, spec)?;
    Ok(LinkKind::UdpForward { host, port })
}

/// The links a `uxduck forward` command line asks for.
///
/// # Errors
/// The first spec that does not parse.
pub fn forward_links(
    inputs: &[String],
    outputs: &[String],
    read_only: bool,
    vehicles: &[String],
) -> Result<Vec<LinkAddParams>, String> {
    let policy = if read_only {
        LinkPolicy::Read
    } else {
        LinkPolicy::ReadWrite
    };
    let mut links = Vec::new();
    for spec in inputs {
        links.push(LinkAddParams {
            name: None,
            kind: input(spec)?,
            policy: Some(policy),
            topology: None,
            vehicle_filter: Vec::new(),
            from_link: None,
            keep_alive: false,
        });
    }
    for spec in outputs {
        links.push(LinkAddParams {
            name: None,
            kind: output(spec)?,
            policy: Some(policy),
            topology: None,
            vehicle_filter: vehicles.to_vec(),
            from_link: None,
            keep_alive: false,
        });
    }
    Ok(links)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_parse_into_link_kinds() {
        assert_eq!(
            input("udp:0.0.0.0:14550"),
            Ok(LinkKind::UdpListen {
                host: "0.0.0.0".into(),
                port: 14550
            })
        );
        assert_eq!(
            input("serial:/dev/ttyAMA0@57600"),
            Ok(LinkKind::Serial {
                device: "/dev/ttyAMA0".into(),
                baud: 57600,
                usb: None
            })
        );
        assert_eq!(
            input("serial:COM3"),
            Ok(LinkKind::Serial {
                device: "COM3".into(),
                baud: DEFAULT_BAUD,
                usb: None
            })
        );
        assert_eq!(
            input("tcp:[::1]:5760"),
            Ok(LinkKind::TcpClient {
                host: "::1".into(),
                port: 5760
            })
        );
        assert_eq!(
            output("udp:192.168.1.20:14550"),
            Ok(LinkKind::UdpForward {
                host: "192.168.1.20".into(),
                port: 14550
            })
        );
        assert_eq!(
            input("udpout:192.168.4.1:14550"),
            Ok(LinkKind::UdpClient {
                host: "192.168.4.1".into(),
                port: 14550
            })
        );
        assert_eq!(
            input("tcpin:0.0.0.0:5760"),
            Ok(LinkKind::TcpServer {
                host: "0.0.0.0".into(),
                port: 5760
            })
        );
        assert!(input("udp:14550").is_err());
        assert!(input("serial:/dev/x@fast").is_err());
        assert!(output("tcp:1.2.3.4:5").unwrap_err().contains("UDP for now"));
        assert!(input("radio:x").unwrap_err().contains("unknown link type"));
    }
}

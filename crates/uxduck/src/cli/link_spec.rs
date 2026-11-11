//! Links on the command line, for `uxduck forward`, as pymavlink and
//! MAVProxy write them (the link form reads the same, `linkSpec.ts`):
//!
//! - `udp:0.0.0.0:15010` or `udpin:0.0.0.0:15010` listens on UDP;
//! - `udpout:192.168.4.1:14550` sends to a UDP server and hears its vehicles;
//! - `serial:/dev/ttyACM0`, `serial:/dev/ttyACM0@115200`, `/dev/ttyACM0,115200`
//!   or `COM3,57600` opens a serial device;
//! - `tcp:127.0.0.1:5760` connects to a TCP server;
//! - `tcpin:0.0.0.0:5760` listens for TCP connections;
//! - as `--out`, `udp:192.168.1.20:15010` or `udpout:…` forwards to that address.

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

/// A device path as MAVProxy takes it, without `serial:`: `/dev/…` or `COM3`.
fn is_device(spec: &str) -> bool {
    let com = spec.get(..3).is_some_and(|c| c.eq_ignore_ascii_case("com"))
        && spec[3..]
            .split([',', '@'])
            .next()
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    spec.starts_with('/') || com
}

/// A serial device with its baud rate after `@` or `,`, or the default.
fn serial(rest: &str, spec: &str) -> Result<LinkKind, String> {
    let (device, baud) = match rest.rsplit_once(['@', ',']) {
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

/// A link vehicles are heard on (`--in`).
///
/// # Errors
/// A message naming what is wrong with the spec.
pub fn input(spec: &str) -> Result<LinkKind, String> {
    if is_device(spec) {
        return serial(spec, spec);
    }
    let (scheme, rest) = spec.split_once(':').ok_or_else(|| {
        format!("{spec}: expected udp:, udpin:, udpout:, serial:, tcp:, tcpin: or a device path")
    })?;
    match scheme {
        "udp" | "udpin" => {
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
        "serial" => serial(rest, spec),
        other => Err(format!(
            "{spec}: unknown link type {other}; use udp, udpin, udpout, serial, tcp or tcpin"
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
    if !matches!(scheme, "udp" | "udpout") {
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
            carries: uxduck_contracts::dto::link::LinkCarries::Mavlink,
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
            carries: uxduck_contracts::dto::link::LinkCarries::Mavlink,
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
        assert_eq!(input("udpin:0.0.0.0:14550"), input("udp:0.0.0.0:14550"));
        // MAVProxy's device forms, without `serial:`.
        assert_eq!(
            input("/dev/ttyUSB0,57600"),
            Ok(LinkKind::Serial {
                device: "/dev/ttyUSB0".into(),
                baud: 57600,
                usb: None
            })
        );
        assert_eq!(
            input("com3,57600"),
            Ok(LinkKind::Serial {
                device: "com3".into(),
                baud: 57600,
                usb: None
            })
        );
        assert_eq!(
            input("serial:/dev/ttyUSB0,57600"),
            input("/dev/ttyUSB0@57600")
        );
        assert_eq!(
            output("udpout:192.168.1.20:14550"),
            output("udp:192.168.1.20:14550")
        );
        assert!(input("udp:14550").is_err());
        assert!(
            input("compass:1")
                .unwrap_err()
                .contains("unknown link type")
        );
        assert!(input("serial:/dev/x@fast").is_err());
        assert!(output("tcp:1.2.3.4:5").unwrap_err().contains("UDP for now"));
        assert!(input("radio:x").unwrap_err().contains("unknown link type"));
    }
}

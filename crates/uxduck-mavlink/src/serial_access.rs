//! What the computer says about a serial device that won't open (FB-287):
//! who owns it, its group, whether this account is in that group and in the
//! groups UXDuck runs with now, whether a port built into the computer has
//! anything behind it, and which flight controllers are plugged in. A link
//! that won't open and the nearby search both say it with
//! [`LinkFailure::device_words_with`].

#[cfg(target_os = "linux")]
use uxduck_core::domain::link_failure::DeviceGroup;
use uxduck_core::domain::link_failure::{DeviceFacts, KnownPort, LinkFailure, LinkFailureKind};

use crate::usb_ids::{UsbKind, lookup};

/// What happened to the serial device at `path`, in plain words.
#[must_use]
pub fn words(failure: &LinkFailure, path: &str) -> String {
    failure.device_words_with(path, &facts(path, failure.kind))
}

/// Why this account may not open `path`, and what to do about it.
#[must_use]
pub fn denied(path: &str) -> String {
    words(
        &LinkFailure::device(LinkFailureKind::AccessDenied, String::new()),
        path,
    )
}

/// What the words for `kind` need, read only for the failures that use it.
/// The owner and groups are read on Linux. macOS lets every account use
/// serial ports, and Windows reports a port another program holds as denied.
fn facts(path: &str, kind: LinkFailureKind) -> DeviceFacts {
    use LinkFailureKind as K;
    if !matches!(kind, K::AccessDenied | K::NoHardware | K::DeviceMissing) {
        return DeviceFacts::default();
    }
    let ports = crate::links::serial_ports();
    let boards = ports
        .iter()
        .filter_map(|port| {
            let usb = port.usb.as_ref()?;
            let known = lookup(usb.vendor_id, usb.product_id)?;
            (known.kind == UsbKind::FlightController).then(|| KnownPort {
                device: port.device.clone(),
                name: known.name.to_owned(),
            })
        })
        .collect();
    #[cfg(target_os = "linux")]
    {
        let built_in = path.starts_with("/dev/ttyS");
        let owner_and_group = if kind == K::AccessDenied {
            owner_and_group(path)
        } else {
            DeviceFacts::default()
        };
        DeviceFacts {
            hardware: built_in.then(|| hardware(path)).flatten(),
            usb_ports: if built_in {
                usb_ports(ports.into_iter().map(|port| port.device))
            } else {
                Vec::new()
            },
            boards,
            ..owner_and_group
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (path, ports);
        DeviceFacts {
            boards,
            ..DeviceFacts::default()
        }
    }
}

/// The device's owner, group and permission bits, and whether this account
/// is in that group in the system's list and in the groups UXDuck runs
/// with now.
#[cfg(target_os = "linux")]
fn owner_and_group(path: &str) -> DeviceFacts {
    use nix::unistd::{Gid, Group, Uid, User, getegid, geteuid, getgroups};
    use std::os::unix::fs::MetadataExt;

    let Ok(meta) = std::fs::metadata(path) else {
        return DeviceFacts::default();
    };
    let gid = Gid::from_raw(meta.gid());
    let me = User::from_uid(geteuid()).ok().flatten();
    let group = Group::from_gid(gid)
        .ok()
        .flatten()
        .map(|group| DeviceGroup {
            gid: gid.as_raw(),
            member: me
                .as_ref()
                .is_some_and(|me| me.gid == gid || group.mem.contains(&me.name)),
            current: getegid() == gid || getgroups().is_ok_and(|groups| groups.contains(&gid)),
            name: group.name,
        });
    DeviceFacts {
        owner: User::from_uid(Uid::from_raw(meta.uid()))
            .ok()
            .flatten()
            .map(|user| user.name),
        group,
        mode: meta.mode(),
        ..DeviceFacts::default()
    }
}

/// Whether a serial chip is behind a built-in port such as `/dev/ttyS15`.
#[cfg(target_os = "linux")]
#[must_use]
pub fn hardware(path: &str) -> Option<bool> {
    hardware_in(path, std::path::Path::new("/sys/class/tty"))
}

/// The kernel's port type is 0, unknown, when nothing answered at the
/// port's address. Linux makes ttyS0 to ttyS31 whether or not they exist.
#[cfg(any(target_os = "linux", test))]
fn hardware_in(path: &str, sys_class_tty: &std::path::Path) -> Option<bool> {
    let name = path.strip_prefix("/dev/")?;
    let kind = std::fs::read_to_string(sys_class_tty.join(name).join("type")).ok()?;
    Some(kind.trim().parse::<u32>().ok()? != 0)
}

/// The ports where a flight controller on USB shows up: CDC ACM boards and
/// USB serial adapters.
#[cfg(any(target_os = "linux", test))]
fn usb_ports(devices: impl Iterator<Item = String>) -> Vec<String> {
    let mut ports: Vec<String> = devices
        .filter(|device| device.starts_with("/dev/ttyACM") || device.starts_with("/dev/ttyUSB"))
        .collect();
    ports.sort();
    ports
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "deterministic tests"
    )]

    use super::*;

    /// FB-287's ttyS15: Linux lists it with port type 0 when no chip is
    /// behind it.
    #[test]
    fn a_built_in_port_with_type_zero_has_no_hardware() {
        let sys = std::env::temp_dir().join(format!("uxduck-tty-{}", std::process::id()));
        for (name, kind) in [("ttyS0", "4\n"), ("ttyS15", "0\n")] {
            std::fs::create_dir_all(sys.join(name)).unwrap();
            std::fs::write(sys.join(name).join("type"), kind).unwrap();
        }
        assert_eq!(hardware_in("/dev/ttyS0", &sys), Some(true));
        assert_eq!(hardware_in("/dev/ttyS15", &sys), Some(false));
        assert_eq!(hardware_in("/dev/ttyS3", &sys), None, "unknown, not absent");
        std::fs::remove_dir_all(&sys).unwrap();
    }

    #[test]
    fn usb_ports_are_the_acm_and_usb_ones() {
        let devices = ["/dev/ttyS0", "/dev/ttyUSB0", "/dev/ttyS15", "/dev/ttyACM0"];
        assert_eq!(
            usb_ports(devices.into_iter().map(String::from)),
            vec!["/dev/ttyACM0".to_owned(), "/dev/ttyUSB0".to_owned()]
        );
    }

    /// Only the failures whose words use them read the device and ports.
    #[test]
    fn other_failures_read_nothing() {
        assert_eq!(
            facts("/dev/ttyS15", LinkFailureKind::DeviceBusy),
            DeviceFacts::default()
        );
    }

    /// Read from a real file: its owner and group are this account's, and
    /// UXDuck runs with that group now.
    #[cfg(target_os = "linux")]
    #[test]
    fn owner_and_group_come_from_the_device() {
        use nix::unistd::{User, geteuid};
        let file = std::env::temp_dir().join(format!("uxduck-device-{}", std::process::id()));
        std::fs::write(&file, b"").unwrap();
        let facts = owner_and_group(file.to_str().unwrap());
        std::fs::remove_file(&file).unwrap();
        let me = User::from_uid(geteuid()).unwrap().unwrap();
        assert_eq!(facts.owner.as_deref(), Some(me.name.as_str()));
        let group = facts.group.expect("the file's group has a name");
        assert!(group.current, "{group:?}");
        assert_eq!(facts.mode & 0o600, 0o600, "the owner may read and write it");
        assert_eq!(
            owner_and_group("/dev/uxduck-not-here"),
            DeviceFacts::default()
        );
    }
}

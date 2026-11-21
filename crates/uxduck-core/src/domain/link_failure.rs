//! Why a link failed, keyed on the kind of failure and the operation that
//! met it rather than on the operating system's sentence, which differs
//! between Windows, macOS and Linux (FB-267). The plain words go in the
//! link's `status_detail`, and the system's own message and code are kept
//! for Connection diagnostics.

use std::io;

use serde::{Deserialize, Serialize};

/// What kind of failure it was.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LinkFailureKind {
    /// Nothing answered before the system gave up waiting.
    NoAnswer,
    /// The host answered, but nothing listens on the port.
    Refused,
    /// The other end reset a connection. For UDP on Windows, the port
    /// that a datagram went to had nothing listening.
    Reset,
    /// This computer dropped the connection itself.
    Aborted,
    /// No route to the host, or the host is down.
    HostUnreachable,
    /// This computer has no network that leads there.
    NetworkUnreachable,
    /// The address cannot be used from this computer.
    AddressNotAvailable,
    /// The port is taken on this computer.
    AddressInUse,
    /// The host name could not be looked up.
    NameNotFound,
    /// A serial device that isn't there.
    DeviceMissing,
    /// A serial device that is not there or is held by another program or
    /// account: on Windows the serial library does not tell these apart.
    DeviceUnavailable,
    /// A serial device another program or account holds, or that this
    /// account may not open.
    DeviceBusy,
    /// Something this account may not open or use: a serial device, or a
    /// socket a firewall or security software refuses.
    AccessDenied,
    /// A serial device with nothing working behind it: opening it fails
    /// with an I/O error, or no such device (FB-287).
    NoHardware,
    /// Anything else: shown in the system's own words.
    #[default]
    Other,
}

/// What the link was doing when it failed, so the words fit it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LinkOperation {
    /// Opening a TCP connection.
    #[default]
    Connect,
    /// Using a TCP connection that was open.
    Connected,
    /// Looking up a host name.
    Lookup,
    /// Opening a socket that listens on this computer.
    Bind,
    /// Setting up this computer's own socket for a connection out.
    Socket,
    /// Sending or receiving a UDP datagram.
    Datagram,
    /// Opening or reading a serial device.
    Device,
}

/// A link's failure as the operating system reported it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkFailure {
    pub kind: LinkFailureKind,
    #[serde(default)]
    pub operation: LinkOperation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_code: Option<i32>,
    /// The system's own message, kept whole.
    pub message: String,
}

impl LinkFailure {
    #[must_use]
    pub fn from_io(operation: LinkOperation, error: &io::Error) -> Self {
        Self {
            kind: LinkFailureKind::of(error.kind(), error.raw_os_error()),
            operation,
            os_code: error.raw_os_error(),
            message: error.to_string(),
        }
    }

    /// A host name that could not be looked up, whatever the lookup said.
    #[must_use]
    pub fn name_not_found(error: &io::Error) -> Self {
        Self {
            kind: LinkFailureKind::NameNotFound,
            ..Self::from_io(LinkOperation::Lookup, error)
        }
    }

    /// A serial device's failure, of a kind its library already told.
    #[must_use]
    pub fn device(kind: LinkFailureKind, message: String) -> Self {
        Self {
            kind,
            operation: LinkOperation::Device,
            os_code: None,
            message,
        }
    }

    /// What happened at `host`:`port`, in plain words for the operation,
    /// or the system's own message where no words fit.
    #[must_use]
    pub fn words(&self, host: &str, port: u16) -> String {
        use LinkFailureKind as K;
        use LinkOperation as O;
        let at = endpoint(host, port);
        match (self.kind, self.operation) {
            (K::AccessDenied, O::Bind) => format!(
                "This computer didn't let UXDuck listen on {at}. Check that a firewall or security software isn't blocking it, or use another port."
            ),
            (K::AccessDenied, _) => format!(
                "This computer didn't let UXDuck open a connection to {at}. Check that a firewall or security software isn't blocking it."
            ),
            (_, O::Socket) => {
                format!("UXDuck couldn't open a network socket on this computer for {at}.")
            }
            (K::NoAnswer, O::Connected) => format!("{at} stopped answering."),
            (K::NoAnswer, _) => format!(
                "Nothing answered at {at}. Check that the device is on and on a network this computer can reach."
            ),
            (K::Refused | K::Reset, O::Datagram) => format!("Nothing is listening at {at}."),
            (K::Refused, _) => format!("{at} refused the connection. Nothing is listening there."),
            (K::Reset, _) => format!("{at} closed the connection."),
            (K::Aborted, _) => format!("This computer dropped the connection to {at}."),
            (K::HostUnreachable, _) => format!(
                "{at} can't be reached from this computer. Check that both are on the same network."
            ),
            (K::NetworkUnreachable, _) => format!(
                "This computer has no network that reaches {at}. Check its network connection."
            ),
            (K::AddressNotAvailable, O::Bind) => {
                format!("{at} can't be used on this computer. Check the address.")
            }
            (K::AddressNotAvailable, _) => {
                format!("{at} can't be used from this computer. Check the address.")
            }
            (K::AddressInUse, O::Bind) => {
                format!("Port {port} is already in use on this computer.")
            }
            (_, O::Bind) => format!("UXDuck couldn't listen on {at}."),
            (K::NameNotFound, _) => {
                format!(
                    "UXDuck couldn't find {host} (port {port}). Check the name, or use its IP address."
                )
            }
            _ => self.message.clone(),
        }
    }

    /// What happened to a serial device, in plain words, or the system's
    /// own message after its path.
    #[must_use]
    pub fn device_words(&self, device: &str) -> String {
        self.device_words_with(device, &DeviceFacts::default())
    }

    /// What happened to a serial device, with what UXDuck could learn about
    /// it: why this account may not open it, whether anything is behind it,
    /// and where the vehicle may be instead (FB-287).
    #[must_use]
    pub fn device_words_with(&self, device: &str, facts: &DeviceFacts) -> String {
        use LinkFailureKind as K;
        let no_hardware = facts.hardware == Some(false);
        let words = match self.kind {
            K::DeviceMissing => {
                format!("{device} isn't connected. Check the cable and that the device is on.")
            }
            K::DeviceUnavailable => format!(
                "UXDuck can't open {device}. It may be unplugged or in use by another program, or this account may not be allowed to open it."
            ),
            K::DeviceBusy => format!(
                "UXDuck can't open {device}. Another program or account may be holding it, or this account may not be allowed to use it."
            ),
            K::AccessDenied if no_hardware => format!(
                "UXDuck isn't allowed to open {device}, and no serial hardware is behind it, so no vehicle can be there."
            ),
            K::AccessDenied => {
                format!(
                    "UXDuck isn't allowed to open {device}. {}",
                    facts.why_denied()
                )
            }
            K::NoHardware if no_hardware => {
                format!("{device} has no serial hardware behind it, so no vehicle can be there.")
            }
            K::NoHardware => format!(
                "Nothing answers at {device}. The system couldn't open it, which usually means no hardware is behind it."
            ),
            _ => return format!("{device}: {}", self.message),
        };
        let look_elsewhere = match self.kind {
            K::DeviceMissing | K::NoHardware => true,
            K::AccessDenied => no_hardware || device.starts_with("/dev/ttyS"),
            _ => false,
        };
        match facts.instead(device).filter(|_| look_elsewhere) {
            Some(instead) => format!("{words} {instead}"),
            None => words,
        }
    }

    /// What it was, without where: for a feedback report, which must not
    /// carry the address a person configured.
    #[must_use]
    pub fn summary(&self) -> String {
        let code = self
            .os_code
            .map_or_else(String::new, |code| format!(", OS error {code}"));
        format!("{:?} during {:?}{code}", self.kind, self.operation)
    }
}

/// What UXDuck could learn about a serial device that won't open, so the
/// words say why and what to do (FB-287). The daemon reads it on Linux, and
/// it stays empty where nothing could be read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceFacts {
    /// The account that owns the device.
    pub owner: Option<String>,
    pub group: Option<DeviceGroup>,
    /// The device's permission bits.
    pub mode: u32,
    /// For a port built into the computer, whether a serial chip answers
    /// behind it: Linux lists 32 of them whether or not they exist.
    pub hardware: Option<bool>,
    /// Ports where a flight controller on USB shows up on this computer.
    pub usb_ports: Vec<String>,
    /// Flight controllers UXDuck knows by their USB id, plugged in now.
    pub boards: Vec<KnownPort>,
}

/// A serial port with a flight controller UXDuck knows behind it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownPort {
    pub device: String,
    pub name: String,
}

/// The group that owns a serial device, and this account's place in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceGroup {
    pub name: String,
    /// 0 is the superuser's group and 5 usually terminals', which nobody
    /// should join to reach a port.
    pub gid: u32,
    /// The system's group list names this account.
    pub member: bool,
    /// One of the groups UXDuck runs with now. A process keeps the groups
    /// it started with, and UXDuck keeps running in the background after
    /// its window closes and after a log out (FB-289).
    pub current: bool,
}

/// The usual id of the tty group, which owns terminals and consoles.
const TTY_GID: u32 = 5;

/// A group name that goes into a typed command as it is: the shadow
/// utilities' own rule, so nothing in it reaches the shell (FB-287).
fn typable_group(name: &str) -> bool {
    let body = name.strip_suffix('$').unwrap_or(name);
    let mut chars = body.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

impl DeviceFacts {
    /// Why this account may not open the device, and what to do about it.
    fn why_denied(&self) -> String {
        let Some(group) = &self.group else {
            return if cfg!(windows) {
                "Close any program using it, or install the board's driver.".to_owned()
            } else {
                "Check that this account may use serial ports.".to_owned()
            };
        };
        let name = &group.name;
        let owner = self.owner.as_deref().unwrap_or("its owner");
        if group.gid == 0 {
            return format!(
                "Only {owner} and the {name} group may open it. A udev rule can give a group such as dialout read and write access to it, and then you can join that group."
            );
        }
        if name == "tty" || group.gid == TTY_GID {
            return "It belongs to the tty group, so it's a terminal, such as the computer's login console on a serial port. Joining that group would let this account write to everyone's terminals, so turn off the serial console instead. On a Raspberry Pi, run sudo raspi-config, choose Interface Options, then Serial Port, answer No to a login shell and Yes to the serial port hardware, and restart.".to_owned();
        }
        if self.mode & 0o060 != 0o060 {
            return format!(
                "Only {owner} may open it, so joining its group, {name}, won't help. A udev rule can give the {name} group read and write access to it."
            );
        }
        let typable = typable_group(name);
        match (group.member, group.current) {
            (_, true) => format!(
                "It belongs to the {name} group, and UXDuck runs in it, so something else is blocking it, such as a security policy."
            ),
            (true, false) => {
                let restart = if typable {
                    format!(
                        "Restart the computer, or run uxduck daemon stop && sg {name} -c \"uxduck daemon start\" in a terminal."
                    )
                } else {
                    "Restart the computer.".to_owned()
                };
                format!(
                    "It belongs to the {name} group. This account joined that group after UXDuck started, and UXDuck keeps the groups it started with, even after a log out, because it runs in the background. {restart}"
                )
            }
            (false, false) => {
                let join = if typable {
                    format!("Run sudo usermod -aG {name} $USER")
                } else {
                    format!("Add this account to the \u{201c}{name}\u{201d} group")
                };
                format!(
                    "It belongs to the {name} group, and this account isn't in it. {join}, then restart the computer. Logging out isn't enough while UXDuck keeps running in the background."
                )
            }
        }
    }

    /// Where the vehicle may be instead of `device`: a flight controller
    /// UXDuck knows, any USB serial port, or where one usually shows up on
    /// Linux.
    fn instead(&self, device: &str) -> Option<String> {
        let mut seen: Vec<(&str, Vec<&str>)> = Vec::new();
        for board in self.boards.iter().filter(|board| board.device != device) {
            match seen.iter_mut().find(|(name, _)| *name == board.name) {
                Some((_, devices)) => devices.push(&board.device),
                None => seen.push((&board.name, vec![&board.device])),
            }
        }
        if !seen.is_empty() {
            let boards = seen
                .iter()
                .map(|(name, devices)| format!("{name} at {}", devices.join(" or ")))
                .collect::<Vec<_>>()
                .join(" and ");
            return Some(format!(
                "UXDuck sees {boards}, so that may be the port you want."
            ));
        }
        let usb = self.usb_ports.join(" or ");
        if !usb.is_empty() {
            return Some(format!(
                "A flight controller on USB shows up as {usb} here, so that may be the port you want."
            ));
        }
        (cfg!(target_os = "linux") && device.starts_with("/dev/ttyS")).then(|| {
            "A flight controller on USB usually shows up as /dev/ttyACM0 or /dev/ttyUSB0."
                .to_owned()
        })
    }
}

/// Why a listener can't bind at a link's host and port, in plain words, with
/// what the system said for diagnostics. The same whether UXDuck checks
/// before it saves the link or opens it (FB-267).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindRefused {
    pub words: String,
    /// Absent when the address was never tried, such as a host that is not
    /// an IP address.
    pub failure: Option<LinkFailure>,
}

impl BindRefused {
    /// Whether another program holds the port.
    #[must_use]
    pub fn in_use(&self) -> bool {
        self.failure
            .as_ref()
            .is_some_and(|failure| failure.kind == LinkFailureKind::AddressInUse)
    }
}

/// `host:port`, with an IPv6 address in brackets so its port reads apart.
#[must_use]
pub fn endpoint(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

impl LinkFailureKind {
    /// The kind first, as the standard library reads it on this platform,
    /// then the code: Windows' socket codes, so a report from Windows reads
    /// the same anywhere, and the host-down code Rust has no kind for.
    #[must_use]
    pub fn of(kind: io::ErrorKind, os_code: Option<i32>) -> Self {
        use io::ErrorKind as K;
        match kind {
            K::TimedOut => Self::NoAnswer,
            K::ConnectionRefused => Self::Refused,
            K::ConnectionReset => Self::Reset,
            K::ConnectionAborted => Self::Aborted,
            K::HostUnreachable => Self::HostUnreachable,
            K::NetworkUnreachable | K::NetworkDown => Self::NetworkUnreachable,
            K::AddrNotAvailable => Self::AddressNotAvailable,
            K::AddrInUse => Self::AddressInUse,
            K::PermissionDenied => Self::AccessDenied,
            _ => os_code.map_or(Self::Other, Self::of_code),
        }
    }

    fn of_code(code: i32) -> Self {
        match code {
            WSAETIMEDOUT => Self::NoAnswer,
            WSAECONNREFUSED => Self::Refused,
            WSAECONNRESET => Self::Reset,
            WSAECONNABORTED => Self::Aborted,
            WSAEHOSTDOWN | WSAEHOSTUNREACH => Self::HostUnreachable,
            WSAENETDOWN | WSAENETUNREACH => Self::NetworkUnreachable,
            WSAEADDRNOTAVAIL => Self::AddressNotAvailable,
            WSAEADDRINUSE => Self::AddressInUse,
            WSAEACCES => Self::AccessDenied,
            WSAHOST_NOT_FOUND | WSATRY_AGAIN | WSANO_DATA => Self::NameNotFound,
            #[cfg(unix)]
            EHOSTDOWN => Self::HostUnreachable,
            _ => Self::Other,
        }
    }
}

// Windows Sockets error codes (winerror.h).
const WSAEACCES: i32 = 10013;
const WSAEADDRINUSE: i32 = 10048;
const WSAEADDRNOTAVAIL: i32 = 10049;
const WSAENETDOWN: i32 = 10050;
const WSAENETUNREACH: i32 = 10051;
const WSAECONNABORTED: i32 = 10053;
const WSAECONNRESET: i32 = 10054;
const WSAETIMEDOUT: i32 = 10060;
const WSAECONNREFUSED: i32 = 10061;
const WSAEHOSTDOWN: i32 = 10064;
const WSAEHOSTUNREACH: i32 = 10065;
const WSAHOST_NOT_FOUND: i32 = 11001;
const WSATRY_AGAIN: i32 = 11002;
const WSANO_DATA: i32 = 11004;

/// "Host is down", which Rust gives no kind of its own.
#[cfg(target_os = "linux")]
const EHOSTDOWN: i32 = 112;
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
const EHOSTDOWN: i32 = 64;
#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd"
    ))
))]
const EHOSTDOWN: i32 = i32::MIN;

#[cfg(test)]
mod tests {
    use super::{DeviceFacts, DeviceGroup, KnownPort, LinkFailure, LinkFailureKind, LinkOperation};
    use std::io;

    /// A character device: root's, in `group`, readable and writable by
    /// owner and group, and this account's place in that group.
    fn device(group: &str, member: bool, current: bool) -> DeviceFacts {
        DeviceFacts {
            owner: Some("root".into()),
            group: Some(DeviceGroup {
                name: group.into(),
                gid: 20,
                member,
                current,
            }),
            mode: 0o020_660,
            ..DeviceFacts::default()
        }
    }

    fn said(kind: LinkFailureKind, facts: &DeviceFacts, path: &str) -> String {
        LinkFailure::device(kind, "the system's words".into()).device_words_with(path, facts)
    }

    fn denied(facts: &DeviceFacts, path: &str) -> String {
        said(LinkFailureKind::AccessDenied, facts, path)
    }

    /// Where a flight controller usually shows up, said only on Linux, whose
    /// names these are.
    const USUAL: &str = if cfg!(target_os = "linux") {
        " A flight controller on USB usually shows up as /dev/ttyACM0 or /dev/ttyUSB0."
    } else {
        ""
    };

    /// FB-287's computer: ArduPilot's two USB ports.
    fn ardupilot() -> Vec<KnownPort> {
        ["/dev/ttyACM0", "/dev/ttyACM1"]
            .into_iter()
            .map(|device| KnownPort {
                device: device.into(),
                name: "ArduPilot flight controller".into(),
            })
            .collect()
    }

    fn failure(operation: LinkOperation, code: i32) -> LinkFailure {
        LinkFailure::from_io(operation, &io::Error::from_raw_os_error(code))
    }

    fn connect(code: i32) -> LinkFailure {
        failure(LinkOperation::Connect, code)
    }

    /// The two errors in FB-267's screenshot, as Windows reports them.
    #[test]
    fn windows_timeout_and_refusal_read_plainly_anywhere() {
        let timeout = connect(10060);
        assert_eq!(timeout.kind, LinkFailureKind::NoAnswer);
        assert_eq!(timeout.os_code, Some(10060));
        assert!(
            timeout
                .words("192.168.50.10", 5760)
                .starts_with("Nothing answered at 192.168.50.10:5760."),
        );
        let refused = connect(10061);
        assert_eq!(refused.kind, LinkFailureKind::Refused);
        assert_eq!(
            refused.words("192.168.50.10", 5760),
            "192.168.50.10:5760 refused the connection. Nothing is listening there."
        );
    }

    #[test]
    fn windows_socket_codes_map_by_code() {
        for (code, kind) in [
            (10013, LinkFailureKind::AccessDenied),
            (10048, LinkFailureKind::AddressInUse),
            (10049, LinkFailureKind::AddressNotAvailable),
            (10050, LinkFailureKind::NetworkUnreachable),
            (10051, LinkFailureKind::NetworkUnreachable),
            (10053, LinkFailureKind::Aborted),
            (10054, LinkFailureKind::Reset),
            (10064, LinkFailureKind::HostUnreachable),
            (10065, LinkFailureKind::HostUnreachable),
            (11001, LinkFailureKind::NameNotFound),
            (11004, LinkFailureKind::NameNotFound),
        ] {
            assert_eq!(connect(code).kind, kind, "code {code}");
        }
    }

    /// The same failures from Linux and macOS read the same as Windows'.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn unix_timeout_and_refusal_match_windows() {
        #[cfg(target_os = "linux")]
        let (etimedout, econnrefused, ehostunreach, enetunreach, ehostdown) =
            (110, 111, 113, 101, 112);
        #[cfg(target_os = "macos")]
        let (etimedout, econnrefused, ehostunreach, enetunreach, ehostdown) = (60, 61, 65, 51, 64);
        assert_eq!(connect(etimedout).kind, LinkFailureKind::NoAnswer);
        assert_eq!(connect(econnrefused).kind, LinkFailureKind::Refused);
        assert_eq!(connect(ehostunreach).kind, LinkFailureKind::HostUnreachable);
        assert_eq!(
            connect(enetunreach).kind,
            LinkFailureKind::NetworkUnreachable
        );
        assert_eq!(connect(ehostdown).kind, LinkFailureKind::HostUnreachable);
        assert_eq!(
            connect(etimedout).words("192.168.50.10", 5760),
            connect(10060).words("192.168.50.10", 5760),
        );
        assert_eq!(
            connect(econnrefused).words("127.0.0.1", 5760),
            connect(10061).words("127.0.0.1", 5760),
        );
    }

    #[test]
    fn kinds_map_without_a_code() {
        for (kind, expected) in [
            (io::ErrorKind::TimedOut, LinkFailureKind::NoAnswer),
            (io::ErrorKind::ConnectionRefused, LinkFailureKind::Refused),
            (io::ErrorKind::ConnectionReset, LinkFailureKind::Reset),
            (io::ErrorKind::ConnectionAborted, LinkFailureKind::Aborted),
            (
                io::ErrorKind::HostUnreachable,
                LinkFailureKind::HostUnreachable,
            ),
            (
                io::ErrorKind::NetworkDown,
                LinkFailureKind::NetworkUnreachable,
            ),
            (
                io::ErrorKind::AddrNotAvailable,
                LinkFailureKind::AddressNotAvailable,
            ),
            (io::ErrorKind::AddrInUse, LinkFailureKind::AddressInUse),
            (
                io::ErrorKind::PermissionDenied,
                LinkFailureKind::AccessDenied,
            ),
        ] {
            let failure = LinkFailure::from_io(LinkOperation::Connect, &io::Error::from(kind));
            assert_eq!(failure.kind, expected);
        }
    }

    /// The words fit what the link was doing, not only the error.
    #[test]
    fn the_words_fit_the_operation() {
        assert_eq!(
            failure(LinkOperation::Connected, 10060).words("192.168.50.10", 5760),
            "192.168.50.10:5760 stopped answering."
        );
        assert_eq!(
            failure(LinkOperation::Connected, 10053).words("192.168.50.10", 5760),
            "This computer dropped the connection to 192.168.50.10:5760."
        );
        assert_eq!(
            failure(LinkOperation::Connected, 10054).words("192.168.50.10", 5760),
            "192.168.50.10:5760 closed the connection."
        );
        // Winsock reports a UDP port with nothing listening as a reset.
        assert_eq!(
            failure(LinkOperation::Datagram, 10054).words("192.168.50.33", 14550),
            "Nothing is listening at 192.168.50.33:14550."
        );
        assert_eq!(
            failure(LinkOperation::Bind, 10048).words("0.0.0.0", 14550),
            "Port 14550 is already in use on this computer."
        );
        let local_ports = failure(LinkOperation::Connect, 10048);
        assert_eq!(
            local_ports.words("192.168.50.10", 5760),
            local_ports.message,
            "out of local ports is not about the port it connects to"
        );
    }

    /// A socket this computer refused to open reads plainly, whatever the
    /// error, and keeps the system's words for diagnostics.
    #[test]
    fn a_local_socket_failure_reads_plainly() {
        #[cfg(target_os = "linux")]
        assert_eq!(
            failure(LinkOperation::Socket, 13).kind,
            LinkFailureKind::AccessDenied
        );
        let denied = LinkFailure::from_io(
            LinkOperation::Socket,
            &io::Error::from(io::ErrorKind::PermissionDenied),
        );
        assert_eq!(
            denied.words("review-test.invalid", 14550),
            "This computer didn't let UXDuck open a connection to review-test.invalid:14550. Check that a firewall or security software isn't blocking it."
        );
        let odd = LinkFailure::from_io(LinkOperation::Socket, &io::Error::other("no buffer space"));
        assert_eq!(
            odd.words("10.0.0.1", 14550),
            "UXDuck couldn't open a network socket on this computer for 10.0.0.1:14550."
        );
        assert_eq!(odd.message, "no buffer space");
    }

    #[test]
    fn an_ipv6_endpoint_is_bracketed() {
        assert_eq!(
            failure(LinkOperation::Connected, 10054).words("::1", 50415),
            "[::1]:50415 closed the connection."
        );
        assert!(
            connect(10060)
                .words("fe80::1", 5760)
                .starts_with("Nothing answered at [fe80::1]:5760.")
        );
    }

    #[test]
    fn a_lookup_failure_names_the_host() {
        let lookup =
            io::Error::other("failed to lookup address information: Name or service not known");
        let failure = LinkFailure::name_not_found(&lookup);
        assert_eq!(failure.operation, LinkOperation::Lookup);
        assert_eq!(
            failure.words("drone.local", 5760),
            "UXDuck couldn't find drone.local (port 5760). Check the name, or use its IP address."
        );
        assert!(failure.message.contains("Name or service not known"));
    }

    #[test]
    fn a_serial_device_gets_advice_about_the_device() {
        let missing = LinkFailure::device(
            LinkFailureKind::DeviceMissing,
            "No such file or directory".into(),
        );
        assert_eq!(
            missing.device_words("/dev/ttyUSB0"),
            "/dev/ttyUSB0 isn't connected. Check the cable and that the device is on."
        );
        let denied = LinkFailure::device(LinkFailureKind::AccessDenied, "Permission denied".into());
        assert!(
            denied
                .device_words("COM4")
                .starts_with("UXDuck isn't allowed to open COM4.")
        );
        let odd = LinkFailure::device(LinkFailureKind::Other, "Input/output error".into());
        assert_eq!(odd.device_words("COM4"), "COM4: Input/output error");
    }

    #[test]
    fn anything_else_keeps_the_systems_words() {
        let odd = LinkFailure::from_io(
            LinkOperation::Connect,
            &io::Error::other("something unusual"),
        );
        assert_eq!(odd.kind, LinkFailureKind::Other);
        assert_eq!(odd.words("192.168.50.10", 5760), "something unusual");
    }

    #[test]
    fn a_summary_says_what_without_where() {
        assert_eq!(
            connect(10061).summary(),
            "Refused during Connect, OS error 10061"
        );
    }

    /// FB-287: not in the group that owns the port. The words name the
    /// group and give the command with it.
    #[test]
    fn a_denied_port_names_its_group_and_the_command_to_join_it() {
        assert_eq!(
            denied(&device("dialout", false, false), "/dev/ttyACM0"),
            "UXDuck isn't allowed to open /dev/ttyACM0. It belongs to the dialout group, and this account isn't in it. Run sudo usermod -aG dialout $USER, then restart the computer. Logging out isn't enough while UXDuck keeps running in the background."
        );
        assert!(
            denied(&device("uucp", false, false), "/dev/ttyUSB0")
                .contains("Run sudo usermod -aG uucp $USER, then restart the computer.")
        );
    }

    /// FB-289: added to the group and logged out and in, but UXDuck kept
    /// running in the background with the groups it started with.
    #[test]
    fn a_group_joined_after_uxduck_started_says_to_start_it_again() {
        assert_eq!(
            denied(&device("dialout", true, false), "/dev/ttyACM0"),
            "UXDuck isn't allowed to open /dev/ttyACM0. It belongs to the dialout group. This account joined that group after UXDuck started, and UXDuck keeps the groups it started with, even after a log out, because it runs in the background. Restart the computer, or run uxduck daemon stop && sg dialout -c \"uxduck daemon start\" in a terminal."
        );
        assert!(
            denied(&device("dialout", true, true), "/dev/ttyACM0")
                .contains("UXDuck runs in it, so something else is blocking it")
        );
    }

    /// Joining the group can't help when the group may not use the port.
    #[test]
    fn a_port_only_its_owner_may_open_says_joining_wont_help() {
        let owner_only = DeviceFacts {
            mode: 0o020_600,
            ..device("dialout", true, true)
        };
        assert_eq!(
            denied(&owner_only, "/dev/ttyUSB0"),
            "UXDuck isn't allowed to open /dev/ttyUSB0. Only root may open it, so joining its group, dialout, won't help. A udev rule can give the dialout group read and write access to it."
        );
        let mut root_group = device("root", false, false);
        root_group.group.as_mut().unwrap().gid = 0;
        let words = denied(&root_group, "/dev/ttyACM0");
        assert!(words.contains("Only root and the root group may open it."));
        assert!(!words.contains("usermod"), "never join the root group");
    }

    /// A Pi's serial console is root:tty 0620, and joining tty would let
    /// this account write to everyone's terminals. The words say to turn
    /// the console off, whatever the mode, and never to join or give tty
    /// anything (FB-287 review).
    #[test]
    fn a_terminal_says_to_turn_off_the_serial_console() {
        let console = format!(
            "UXDuck isn't allowed to open /dev/ttyS0. It belongs to the tty group, so it's a terminal, such as the computer's login console on a serial port. Joining that group would let this account write to everyone's terminals, so turn off the serial console instead. On a Raspberry Pi, run sudo raspi-config, choose Interface Options, then Serial Port, answer No to a login shell and Yes to the serial port hardware, and restart.{USUAL}"
        );
        for mode in [0o020_620, 0o020_660, 0o020_600] {
            for (name, member) in [("tty", false), ("tty", true)] {
                let facts = DeviceFacts {
                    mode,
                    ..device(name, member, false)
                };
                assert_eq!(denied(&facts, "/dev/ttyS0"), console, "{mode:o} {member}");
            }
        }
        let mut by_id = device("terminals", false, false);
        by_id.group.as_mut().unwrap().gid = 5;
        let words = denied(&by_id, "/dev/ttyAMA0");
        assert!(words.contains("turn off the serial console"), "{words}");
        assert!(!words.contains("usermod") && !words.contains("udev"));
    }

    /// A group name that isn't one the shadow utilities would make goes in
    /// words, never into a command to paste (FB-287 review).
    #[test]
    fn an_odd_group_name_is_never_put_in_a_command() {
        for name in ["evil$(id)", "domain users", "-rf", ""] {
            for member in [false, true] {
                let words = denied(&device(name, member, false), "/dev/ttyACM0");
                assert!(
                    !words.contains("usermod") && !words.contains(" sg "),
                    "{name}: {words}"
                );
            }
        }
        assert!(
            denied(&device("domain users", false, false), "/dev/ttyACM0").contains(
                "Add this account to the “domain users” group, then restart the computer."
            )
        );
        assert!(
            denied(&device("evil$(id)", true, false), "/dev/ttyACM0")
                .ends_with("because it runs in the background. Restart the computer.")
        );
        for name in [
            "dialout",
            "uucp",
            "Serial_Ports",
            "_serial",
            "machine$",
            "plug-dev",
        ] {
            assert!(
                denied(&device(name, false, false), "/dev/ttyACM0")
                    .contains(&format!("Run sudo usermod -aG {name} $USER")),
                "{name}"
            );
        }
    }

    /// A built-in port with hardware, and ArduPilot on USB.
    #[test]
    fn a_built_in_port_points_at_the_flight_controller() {
        let with_board = DeviceFacts {
            hardware: Some(true),
            boards: ardupilot(),
            ..device("dialout", false, false)
        };
        assert_eq!(
            denied(&with_board, "/dev/ttyS4"),
            "UXDuck isn't allowed to open /dev/ttyS4. It belongs to the dialout group, and this account isn't in it. Run sudo usermod -aG dialout $USER, then restart the computer. Logging out isn't enough while UXDuck keeps running in the background. UXDuck sees ArduPilot flight controller at /dev/ttyACM0 or /dev/ttyACM1, so that may be the port you want."
        );
        let usb_only = DeviceFacts {
            boards: Vec::new(),
            usb_ports: vec!["/dev/ttyUSB0".into()],
            ..with_board.clone()
        };
        assert!(denied(&usb_only, "/dev/ttyS4").ends_with(
            "A flight controller on USB shows up as /dev/ttyUSB0 here, so that may be the port you want."
        ));
        assert!(
            !denied(&with_board, "/dev/ttyACM0").contains("UXDuck sees"),
            "the board's own other port would be denied too"
        );
    }

    /// FB-287's first words: ttyS15 with no chip behind it, as this account
    /// may not open it.
    #[test]
    fn a_denied_port_with_no_hardware_says_so_instead() {
        let empty = DeviceFacts {
            hardware: Some(false),
            boards: ardupilot(),
            ..device("dialout", false, false)
        };
        assert_eq!(
            denied(&empty, "/dev/ttyS15"),
            "UXDuck isn't allowed to open /dev/ttyS15, and no serial hardware is behind it, so no vehicle can be there. UXDuck sees ArduPilot flight controller at /dev/ttyACM0 or /dev/ttyACM1, so that may be the port you want."
        );
        let nothing_plugged_in = DeviceFacts {
            boards: Vec::new(),
            ..empty
        };
        assert_eq!(
            denied(&nothing_plugged_in, "/dev/ttyS15"),
            format!(
                "UXDuck isn't allowed to open /dev/ttyS15, and no serial hardware is behind it, so no vehicle can be there.{USUAL}"
            )
        );
    }

    /// FB-287 once in the group: "/dev/ttyS15: I/O error". The words say
    /// nothing answers, with confidence when the kernel says no chip is
    /// there.
    #[test]
    fn a_port_that_fails_with_an_io_error_says_nothing_answers() {
        let unknown = DeviceFacts {
            boards: ardupilot(),
            ..DeviceFacts::default()
        };
        assert_eq!(
            said(LinkFailureKind::NoHardware, &unknown, "/dev/ttyS15"),
            "Nothing answers at /dev/ttyS15. The system couldn't open it, which usually means no hardware is behind it. UXDuck sees ArduPilot flight controller at /dev/ttyACM0 or /dev/ttyACM1, so that may be the port you want."
        );
        let empty = DeviceFacts {
            hardware: Some(false),
            ..DeviceFacts::default()
        };
        assert_eq!(
            said(LinkFailureKind::NoHardware, &empty, "/dev/ttyS15"),
            format!(
                "/dev/ttyS15 has no serial hardware behind it, so no vehicle can be there.{USUAL}"
            )
        );
        assert_eq!(
            said(
                LinkFailureKind::NoHardware,
                &DeviceFacts::default(),
                "/dev/cu.usbmodem1"
            ),
            "Nothing answers at /dev/cu.usbmodem1. The system couldn't open it, which usually means no hardware is behind it."
        );
    }

    /// A port that isn't there names a board that is.
    #[test]
    fn a_missing_port_names_a_flight_controller_that_is_plugged_in() {
        let facts = DeviceFacts {
            boards: ardupilot(),
            ..DeviceFacts::default()
        };
        assert_eq!(
            said(LinkFailureKind::DeviceMissing, &facts, "/dev/ttyS15"),
            "/dev/ttyS15 isn't connected. Check the cable and that the device is on. UXDuck sees ArduPilot flight controller at /dev/ttyACM0 or /dev/ttyACM1, so that may be the port you want."
        );
        assert!(
            !said(LinkFailureKind::DeviceBusy, &facts, "/dev/ttyS15").contains("UXDuck sees"),
            "a busy port is the right one"
        );
    }

    #[test]
    fn device_words_have_no_semicolons() {
        let mut root_group = device("root", false, false);
        root_group.group.as_mut().unwrap().gid = 0;
        for facts in [
            DeviceFacts::default(),
            device("dialout", false, false),
            device("dialout", true, false),
            device("dialout", true, true),
            DeviceFacts {
                mode: 0o600,
                ..device("tty", true, true)
            },
            root_group,
            DeviceFacts {
                hardware: Some(false),
                ..DeviceFacts::default()
            },
            DeviceFacts {
                hardware: Some(true),
                usb_ports: vec!["/dev/ttyACM0".into()],
                ..device("dialout", false, false)
            },
            DeviceFacts {
                boards: ardupilot(),
                ..device("dialout", false, false)
            },
        ] {
            for kind in [
                LinkFailureKind::AccessDenied,
                LinkFailureKind::NoHardware,
                LinkFailureKind::DeviceMissing,
            ] {
                assert!(
                    !said(kind, &facts, "/dev/ttyS15").contains(';'),
                    "{kind:?} {facts:?}"
                );
            }
        }
    }

    #[test]
    fn plain_words_have_no_semicolons() {
        use LinkFailureKind as K;
        use LinkOperation as O;
        for kind in [
            K::NoAnswer,
            K::Refused,
            K::Reset,
            K::Aborted,
            K::HostUnreachable,
            K::NetworkUnreachable,
            K::AddressNotAvailable,
            K::AddressInUse,
            K::NameNotFound,
            K::DeviceMissing,
            K::DeviceUnavailable,
            K::DeviceBusy,
            K::AccessDenied,
            K::NoHardware,
        ] {
            for operation in [
                O::Connect,
                O::Connected,
                O::Lookup,
                O::Bind,
                O::Socket,
                O::Datagram,
                O::Device,
            ] {
                let failure = LinkFailure {
                    kind,
                    operation,
                    os_code: None,
                    message: String::new(),
                };
                assert!(
                    !failure.words("10.0.0.1", 14550).contains(';'),
                    "{kind:?} {operation:?}"
                );
                assert!(!failure.device_words("COM4").contains(';'), "{kind:?}");
            }
        }
    }
}

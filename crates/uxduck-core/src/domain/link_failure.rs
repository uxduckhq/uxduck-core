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
        match self.kind {
            LinkFailureKind::DeviceMissing => {
                format!("{device} isn't connected. Check the cable and that the device is on.")
            }
            LinkFailureKind::DeviceUnavailable => format!(
                "UXDuck can't open {device}. It may be unplugged or in use by another program, or this account may not be allowed to open it."
            ),
            LinkFailureKind::DeviceBusy => format!(
                "UXDuck can't open {device}. Another program or account may be holding it, or this account may not be allowed to use it."
            ),
            LinkFailureKind::AccessDenied => format!(
                "UXDuck isn't allowed to open {device}. Check that this account may use serial ports."
            ),
            _ => format!("{device}: {}", self.message),
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
    use super::{LinkFailure, LinkFailureKind, LinkOperation};
    use std::io;

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

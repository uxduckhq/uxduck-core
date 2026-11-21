//! Finding vehicles (ADR-0018, S44): who is looking and how far, the finds
//! the daemon's probes report, with the vehicle a UID names, kept 10 s past
//! their last sighting and pushed to whoever is looking, and the "Flight
//! controller plugged in" notice. Finding never connects anything (F8).

use std::collections::BTreeMap;

use serde_json::{Value, json};
use uxduck_contracts::dto::Empty;
use uxduck_contracts::dto::find::{Find as FindDto, FindResult, FindStartParams};
use uxduck_contracts::dto::link::{SerialDetectParams, SerialDetectProgress, SerialDetectResult};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::{ClientRequest, ServerNotification};
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::find::{Find, FindScope};
use uxduck_core::domain::link::{LinkKind, UsbId};
use uxduck_core::domain::notification::{
    Action, NotificationAction, NotificationKind, Severity, Subject,
};

use crate::commands::Raise;
use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// A find not seen again for this long is gone (F7).
pub const FIND_LIFETIME_US: u64 = 10_000_000;

/// What the probes should look at now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Look {
    pub scope: FindScope,
    /// Serial devices the known-id table does not know too.
    pub all_serial: bool,
    /// What links already use, which a search leaves alone (F6).
    pub owned: Owned,
    /// The system id a probe's hello comes from: the daemon's setting.
    pub gcs_system_id: u8,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Owned {
    pub devices: Vec<String>,
    pub usb: Vec<UsbId>,
    pub udp_ports: Vec<u16>,
    pub tcp: Vec<(String, u16)>,
}

/// The daemon's probes (ADR-0015 L9, F13).
pub trait DiscoveryPort: Send + std::fmt::Debug {
    /// Looks as asked until called again; `None` stops, holding nothing.
    fn look(&mut self, look: Option<Look>);
    /// The subnets a network search covers, such as `192.168.1.0/24`.
    fn subnets(&self) -> Vec<String> {
        Vec::new()
    }
    /// Listens to a serial device for its baud rate, writing nothing,
    /// telling each rate through `Engine::baud_listening`, and answers
    /// through `Engine::baud_detected`.
    fn detect_baud(&mut self, device: String);
    /// Stops listening to a device and lets it go: `Engine::baud_detected`
    /// is then told it stopped.
    fn stop_detecting(&mut self, device: &str);
}

#[derive(Debug, Default)]
pub(crate) struct Finding {
    port: Option<Box<dyn DiscoveryPort>>,
    finds: BTreeMap<String, Find>,
    /// Each looking client's scope, and whether it asked for every serial
    /// port; `once` for `find/vehicles`, which ends after a round.
    lookers: BTreeMap<ConnectionId, Looker>,
    current: Option<Look>,
    /// Devices being listened to for their baud rate, with who asked: a
    /// search leaves them alone meanwhile.
    detecting: BTreeMap<String, Detecting>,
}

#[derive(Clone, Debug)]
struct Looker {
    scope: FindScope,
    all_serial: bool,
    /// A `find/vehicles` request to answer after the next whole round.
    answer: Option<Value>,
}

fn scope_capability(scope: FindScope) -> Capability {
    if scope == FindScope::Network {
        Capability::Config
    } else {
        Capability::Read
    }
}

/// A device being listened to for its baud rate.
#[derive(Clone, Debug)]
struct Detecting {
    connection: ConnectionId,
    request: Value,
    /// Its USB identity when it started, for a link that finds it by that.
    usb: Option<UsbId>,
}

/// Whether a serial link's device is `device`, by its path or by the USB
/// identity it has, as a search for vehicles matches them.
fn same_device(kind: &LinkKind, device: &str, usb: Option<&UsbId>) -> bool {
    let LinkKind::Serial {
        device: path,
        usb: by,
        ..
    } = kind
    else {
        return false;
    };
    path == device
        || by.as_ref().zip(usb).is_some_and(|(by, usb)| {
            by.vendor_id == usb.vendor_id
                && by.product_id == usb.product_id
                && (by.serial_number.is_none() || by.serial_number == usb.serial_number)
        })
}

impl Engine {
    /// Starts listening to a serial device for its baud rate, answered
    /// later. A device a link that is on uses is refused. A flight
    /// controller on native USB, which ignores the rate, or in its
    /// bootloader, which speaks no MAVLink, is answered at once unopened.
    fn detect_baud(&mut self, device: &str) -> Result<Value, ApiError> {
        use uxduck_core::domain::link::LinkStatus;
        use uxduck_mavlink::usb_ids::{UsbKind, lookup};
        if device.is_empty() {
            return Err(ApiError::new(
                ErrorKind::InvalidParams,
                "Choose a serial device first.",
            ));
        }
        let usb = self
            .system
            .serial_ports()
            .into_iter()
            .find(|p| p.device == device)
            .and_then(|p| p.usb);
        let held = self
            .state
            .links
            .values()
            .filter(|link| {
                !matches!(
                    link.status,
                    LinkStatus::Disabled
                        | LinkStatus::DisabledByLoopDetection
                        | LinkStatus::Stopped
                )
            })
            .find_map(|link| match &link.kind {
                LinkKind::Serial { baud, .. } if same_device(&link.kind, device, usb.as_ref()) => {
                    Some((link.name.clone(), *baud))
                }
                _ => None,
            });
        if let Some((name, baud)) = held {
            return Err(ApiError::new(
                ErrorKind::Busy,
                format!(
                    "{name} already uses this device at {baud} baud. Turn that link off to listen for its rate."
                ),
            ));
        }
        match usb.as_ref().and_then(|u| lookup(u.vendor_id, u.product_id)) {
            Some(known) if known.kind == UsbKind::FlightController => {
                return to_value(&SerialDetectResult::NativeUsb {
                    name: known.name.to_owned(),
                });
            }
            Some(known) if known.kind == UsbKind::Bootloader => {
                return to_value(&SerialDetectResult::Bootloader {
                    name: known.name.to_owned(),
                });
            }
            _ => {}
        }
        if self.finding.port.is_none() {
            return Err(ApiError::new(
                ErrorKind::InvalidRequest,
                "Detecting a baud rate needs UXDuck Desktop.",
            ));
        }
        if self.finding.detecting.contains_key(device) {
            return Err(ApiError::new(
                ErrorKind::Busy,
                "UXDuck is already listening to this device. Wait for it to finish.",
            ));
        }
        let Some((connection, request)) = self.replying_to.take() else {
            return Err(ApiError::new(
                ErrorKind::InvalidRequest,
                "Detecting a baud rate needs a request to answer.",
            ));
        };
        self.finding.detecting.insert(
            device.to_owned(),
            Detecting {
                connection,
                request,
                usb,
            },
        );
        self.relook();
        if let Some(port) = self.finding.port.as_mut() {
            port.detect_baud(device.to_owned());
        }
        Ok(Value::Null)
    }

    /// Stops listening to `device` for its rate, if anything is: its
    /// request is answered `stopped` once the device is let go.
    fn stop_detecting(&mut self, device: &str) {
        if !self.finding.detecting.contains_key(device) {
            return;
        }
        if let Some(port) = self.finding.port.as_mut() {
            port.stop_detecting(device);
        }
    }

    /// A link is about to open `kind`: listening to the same device stops,
    /// so the link has it.
    pub(crate) fn stop_detecting_for_link(&mut self, kind: &LinkKind) {
        let devices: Vec<String> = self
            .finding
            .detecting
            .iter()
            .filter(|(device, d)| same_device(kind, device, d.usb.as_ref()))
            .map(|(device, _)| device.clone())
            .collect();
        for device in devices {
            self.stop_detecting(&device);
        }
    }

    /// Listening to `device` moved to `baud`: who asked is told.
    pub fn baud_listening(&mut self, device: &str, baud: u32) {
        let Some(connection) = self.finding.detecting.get(device).map(|d| d.connection) else {
            return;
        };
        self.send(
            connection,
            &json!({
                "jsonrpc": "2.0",
                "method": "serial/detectProgress",
                "params": SerialDetectProgress { device: device.to_owned(), baud },
            }),
        );
    }

    /// Listening to `device` for its baud rate ended: who asked is answered.
    pub fn baud_detected(&mut self, device: &str, result: &SerialDetectResult) {
        let Some(Detecting {
            connection,
            request,
            ..
        }) = self.finding.detecting.remove(device)
        else {
            return;
        };
        let result = to_value(result).unwrap_or(Value::Null);
        self.send(
            connection,
            &json!({ "jsonrpc": "2.0", "id": request, "result": result }),
        );
        self.relook();
    }

    /// The daemon's probes; the browser's daemon has none.
    pub fn set_discovery(&mut self, port: Box<dyn DiscoveryPort>) {
        self.finding.port = Some(port);
    }

    fn owned(&self) -> Owned {
        let mut owned = Owned {
            devices: self.finding.detecting.keys().cloned().collect(),
            ..Owned::default()
        };
        for link in self.state.links.values() {
            match &link.kind {
                LinkKind::Serial { device, usb, .. } => {
                    owned.devices.push(device.clone());
                    owned.usb.extend(usb.clone());
                }
                LinkKind::UdpListen { port, .. } => owned.udp_ports.push(*port),
                LinkKind::TcpClient { host, port } => owned.tcp.push((host.clone(), *port)),
                // Our own server: never probed as if it were SITL's.
                LinkKind::TcpServer { port, .. } => owned.tcp.push(("127.0.0.1".into(), *port)),
                _ => {}
            }
        }
        owned
    }

    /// Tells the probes what to look at: the widest scope anyone asked for.
    pub(crate) fn relook(&mut self) {
        let look = self
            .finding
            .lookers
            .values()
            .map(|l| (l.scope, l.all_serial))
            .reduce(|a, b| (a.0.max(b.0), a.1 || b.1))
            .map(|(scope, all_serial)| Look {
                scope,
                all_serial,
                owned: self.owned(),
                gcs_system_id: self.state.settings.gcs_system_id,
            });
        if look == self.finding.current {
            return;
        }
        self.finding.current.clone_from(&look);
        if look.is_none() {
            self.finding.finds.clear();
        }
        if let Some(port) = self.finding.port.as_mut() {
            port.look(look);
        }
    }

    /// The subnets a network search would cover, for its approval.
    pub(crate) fn finding_subnets(&self) -> Vec<String> {
        self.finding
            .port
            .as_ref()
            .map(|p| p.subnets())
            .unwrap_or_default()
    }

    fn find_result(&self) -> FindResult {
        FindResult {
            finds: self.finding.finds.values().map(FindDto::from).collect(),
            subnets: self
                .finding
                .port
                .as_ref()
                .map(|p| p.subnets())
                .unwrap_or_default(),
        }
    }

    fn push_finds(&mut self) {
        let frame = json!({
            "jsonrpc": "2.0",
            "method": "find/changed",
            "params": self.find_result(),
        });
        debug_assert!(
            serde_json::from_value::<ServerNotification>(json!({
                "method": "find/changed",
                "params": frame["params"],
            }))
            .is_ok()
        );
        let looking: Vec<ConnectionId> = self
            .finding
            .lookers
            .iter()
            .filter(|(_, l)| l.answer.is_none())
            .map(|(c, _)| *c)
            .collect();
        for connection in looking {
            self.send(connection, &frame);
        }
    }

    /// What the probes saw; a vehicle known by its UID gets its name.
    pub fn finds_observed(&mut self, finds: Vec<Find>) {
        if self.finding.current.is_none() {
            return;
        }
        let mut changed = false;
        for mut find in finds {
            if let Some(found) = find.vehicle.as_mut()
                && let Some(id) = &found.vehicle_id
            {
                found.name = self
                    .state
                    .vehicles
                    .values()
                    .find(|v| &v.id == id || v.aliases.contains(id))
                    .map(|v| v.name.clone());
            }
            let before = self.finding.finds.get(&find.id);
            let same = before.is_some_and(|b| {
                let mut b = b.clone();
                b.seen_us = find.seen_us;
                b == find
            });
            changed |= !same;
            self.finding.finds.insert(find.id.clone(), find);
        }
        changed |= self.expire_finds();
        if changed {
            self.push_finds();
        }
    }

    /// Drops finds past their lifetime; whether any went.
    fn expire_finds(&mut self) -> bool {
        let now = self.clock.now_us();
        let before = self.finding.finds.len();
        self.finding
            .finds
            .retain(|_, f| now.saturating_sub(f.seen_us) <= FIND_LIFETIME_US);
        before != self.finding.finds.len()
    }

    /// Once a second while looking.
    pub fn finds_tick(&mut self) {
        if self.expire_finds() {
            self.push_finds();
        }
    }

    /// The probes finished a whole round at this scope: `find/vehicles`
    /// requests that asked no further are answered.
    pub fn find_round_done(&mut self, scope: FindScope) {
        let result = to_value(&self.find_result()).unwrap_or(Value::Null);
        let done: Vec<(ConnectionId, Value)> = self
            .finding
            .lookers
            .iter()
            .filter_map(|(c, l)| {
                l.answer
                    .clone()
                    .filter(|_| l.scope <= scope)
                    .map(|id| (*c, id))
            })
            .collect();
        for (connection, id) in done {
            self.finding.lookers.remove(&connection);
            self.send(
                connection,
                &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            );
        }
        self.relook();
    }

    /// A USB device appeared (F3): a known flight controller that no link
    /// owns gets "Flight controller plugged in" with Connect, once per plug.
    pub fn usb_plugged(&mut self, device: &str, usb: &UsbId, name: &str) {
        let owned = self.owned();
        let same = |u: &UsbId| {
            u.vendor_id == usb.vendor_id
                && u.product_id == usb.product_id
                && (u.serial_number.is_none() || u.serial_number == usb.serial_number)
        };
        if owned.usb.iter().any(same) || owned.devices.iter().any(|d| d == device) {
            return;
        }
        self.raise(Raise {
            severity: Severity::Info,
            kind: NotificationKind::Transient,
            subject: Subject::System,
            title: "Flight controller plugged in".into(),
            body: Some(name.to_owned()),
            actions: vec![NotificationAction {
                label: "Connect".into(),
                action: Action::ConnectSerial {
                    device: device.to_owned(),
                    usb: Some(usb.clone()),
                },
            }],
            condition: None,
            report: None,
        });
    }

    pub(crate) fn stop_finding_for(&mut self, connection: ConnectionId) {
        if self.finding.lookers.remove(&connection).is_some() {
            self.relook();
        }
        let asked: Vec<String> = self
            .finding
            .detecting
            .iter()
            .filter(|(_, d)| d.connection == connection)
            .map(|(device, _)| device.clone())
            .collect();
        for device in asked {
            self.stop_detecting(&device);
        }
    }
}

#[allow(clippy::needless_pass_by_value, reason = "as every handler is called")]
pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    let require = |capability: Capability| {
        if granted.contains(&capability) {
            Ok(())
        } else {
            Err(ApiError::new(
                ErrorKind::CapabilityDenied,
                capability.denied_message(),
            ))
        }
    };
    match request {
        ClientRequest::FindStart(FindStartParams { scope, all_serial }) => {
            let scope: FindScope = scope.into();
            require(scope_capability(scope))?;
            engine.finding.lookers.insert(
                connection,
                Looker {
                    scope,
                    all_serial,
                    answer: None,
                },
            );
            engine.relook();
            to_value(&engine.find_result())
        }
        ClientRequest::FindStop(Empty {}) => {
            engine.stop_finding_for(connection);
            to_value(&Empty {})
        }
        ClientRequest::FindVehicles(FindStartParams { scope, all_serial }) => {
            let scope: FindScope = scope.into();
            require(scope_capability(scope))?;
            if engine.finding.port.is_none() {
                return Err(ApiError::new(
                    ErrorKind::InvalidRequest,
                    "finding vehicles needs UXDuck Desktop",
                ));
            }
            let Some((_, id)) = engine.replying_to.take() else {
                return to_value(&engine.find_result());
            };
            engine.finding.lookers.insert(
                connection,
                Looker {
                    scope,
                    all_serial,
                    answer: Some(id),
                },
            );
            engine.relook();
            // Answered after the probes' next whole round.
            Ok(Value::Null)
        }
        ClientRequest::SerialDetectBaud(SerialDetectParams { device }) => {
            require(Capability::Config)?;
            engine.detect_baud(device.trim())
        }
        ClientRequest::SerialDetectCancel(SerialDetectParams { device }) => {
            require(Capability::Config)?;
            engine.stop_detecting(device.trim());
            to_value(&Empty {})
        }
        _ => Err(ApiError::new(ErrorKind::Internal, "not a find method")),
    }
}

/// The capability a find request needs, for approvals.
#[must_use]
pub(crate) fn needed(request: &ClientRequest) -> Option<Capability> {
    match request {
        ClientRequest::FindStart(p) | ClientRequest::FindVehicles(p) => {
            Some(scope_capability(p.scope.into()))
        }
        _ => None,
    }
}

//! Finding vehicles (ADR-0018, S44): who is looking and how far, the finds
//! the daemon's probes report, with the vehicle a UID names, kept 10 s past
//! their last sighting and pushed to whoever is looking, and the "Flight
//! controller plugged in" notice. Finding never connects anything (F8).

use std::collections::BTreeMap;

use serde_json::{Value, json};
use uxduck_contracts::dto::Empty;
use uxduck_contracts::dto::find::{Find as FindDto, FindResult, FindStartParams};
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
}

#[derive(Debug, Default)]
pub(crate) struct Finding {
    port: Option<Box<dyn DiscoveryPort>>,
    finds: BTreeMap<String, Find>,
    /// Each looking client's scope, and whether it asked for every serial
    /// port; `once` for `find/vehicles`, which ends after a round.
    lookers: BTreeMap<ConnectionId, Looker>,
    current: Option<Look>,
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

impl Engine {
    /// The daemon's probes; the browser's daemon has none.
    pub fn set_discovery(&mut self, port: Box<dyn DiscoveryPort>) {
        self.finding.port = Some(port);
    }

    fn owned(&self) -> Owned {
        let mut owned = Owned::default();
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

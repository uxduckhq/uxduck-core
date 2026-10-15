//! The interface UXDuck serves (S109, ADR-0025 D3): the daemon downloads,
//! checks and keeps interfaces on disk and says which one a page loaded now
//! gets; the engine keeps that status, answers `interface/status`, passes
//! `interface/check` to the daemon and pushes `interface/updated` to every
//! client that reads, so open pages can offer a reload. Never journaled.

use serde_json::Value;
use uxduck_contracts::dto::interface::InterfaceStatus;
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::{ClientRequest, ServerNotification};
use uxduck_core::domain::client::Capability;

use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// The daemon's interface store.
pub trait InterfacePort: Send + std::fmt::Debug {
    /// Checks for a newer interface now. What it finds comes back through
    /// [`Engine::interface_status`].
    fn check(&mut self);
}

#[derive(Debug, Default)]
pub(crate) struct Interface {
    port: Option<Box<dyn InterfacePort>>,
    status: Option<InterfaceStatus>,
}

pub(crate) fn handle(
    engine: &mut Engine,
    request: &ClientRequest,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    if !granted.contains(&Capability::Read) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            Capability::Read.denied_message(),
        ));
    }
    if let ClientRequest::InterfaceCheck(_) = request
        && let Some(port) = engine.interface.port.as_mut()
    {
        port.check();
    }
    match &engine.interface.status {
        Some(status) => to_value(status),
        // UXDuck in the browser (S20) serves no interface of its own.
        None => Err(ApiError::new(
            ErrorKind::NotFound,
            "this UXDuck does not serve an interface",
        )),
    }
}

impl Engine {
    /// The daemon's interface store, which `interface/check` asks.
    pub fn set_interface(&mut self, port: Box<dyn InterfacePort>) {
        self.interface.port = Some(port);
    }

    /// What the daemon serves now. A change goes to every client that
    /// reads as `interface/updated`.
    pub fn interface_status(&mut self, status: InterfaceStatus) {
        if self.interface.status.as_ref() == Some(&status) {
            return;
        }
        self.interface.status = Some(status.clone());
        let frame = crate::rpc::router::notification(&ServerNotification::InterfaceUpdated(status));
        let readers: Vec<ConnectionId> = self
            .connections
            .iter()
            .filter(|(_, c)| {
                c.admitted
                    .as_ref()
                    .is_some_and(|a| a.granted.contains(&Capability::Read))
            })
            .map(|(id, _)| *id)
            .collect();
        for id in readers {
            self.send(id, &frame);
        }
    }
}

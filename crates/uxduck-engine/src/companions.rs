//! A vehicle's companion computers (S296, ADR-0049 O7, O8): added by host
//! name, and checked over SSH by the host through [`CompanionPort`], which
//! answers through [`Engine::companion_checked`]. Only UXDuck Desktop and
//! the daemon have SSH; without a port, adding works and checking is refused
//! with where to do it.

use serde_json::Value;
use uxduck_contracts::dto::companion::{ComputerAddParams, ComputerCheckParams, ComputerParams};
use uxduck_contracts::dto::vehicle::Vehicle as VehicleDto;
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::companion::{
    self, CheckState, CompanionCheck, CompanionComputer, CompanionRole,
};
use uxduck_core::domain::vehicle::Vehicle;
use uxduck_core::events::{Actor, Change};

use crate::{ConnectionId, Engine};

/// What the engine asks the host for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompanionOrder {
    /// Sign in and read its system, architecture and UXDuck, pinning its
    /// host key on the first check and refusing one that changed.
    Check {
        vehicle_id: String,
        computer: Box<CompanionComputer>,
        /// Used for this check alone, to add this computer's UXDuck key.
        password: Option<Password>,
    },
    /// Forget its pinned host key.
    Forget { computer_id: String },
}

/// A password on its way to `ssh`, never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct Password(pub String);

impl std::fmt::Debug for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Password(..)")
    }
}

/// Where companion computers are reached: the daemon's SSH.
pub trait CompanionPort: Send {
    fn request(&mut self, order: CompanionOrder);
}

/// A check's answer: what it found, or why it could not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckAnswer {
    pub vehicle_id: String,
    pub computer_id: String,
    pub result: Result<Found, String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Found {
    pub os: Option<String>,
    pub arch: Option<String>,
    pub uxduck: Option<String>,
    pub host_key: Option<String>,
}

#[derive(Default)]
pub(crate) struct Companions {
    port: Option<Box<dyn CompanionPort>>,
}

impl std::fmt::Debug for Companions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Companions")
            .field("port", &self.port.is_some())
            .finish()
    }
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::InvalidValue, message)
}

fn to_value(vehicle: &Vehicle) -> Result<Value, ApiError> {
    serde_json::to_value(VehicleDto::from(vehicle))
        .map_err(|e| ApiError::new(ErrorKind::Internal, e.to_string()))
}

impl Engine {
    pub fn set_companions(&mut self, port: Box<dyn CompanionPort>) {
        self.companions.port = Some(port);
    }

    fn companion_vehicle(&self, id: &str) -> Result<Vehicle, ApiError> {
        self.vehicle(id)
            .cloned()
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("vehicle {id} not found")))
    }

    fn save_companions(
        &mut self,
        connection: Option<ConnectionId>,
        mut vehicle: Vehicle,
    ) -> Vehicle {
        vehicle.version += 1;
        let change = Change::VehicleUpserted {
            vehicle: vehicle.clone(),
        };
        match connection {
            Some(connection) => self.commit(connection, change),
            None => self.commit_as(Actor::default(), change),
        };
        vehicle
    }

    /// `computer/add`.
    pub(crate) fn add_computer(
        &mut self,
        connection: ConnectionId,
        params: ComputerAddParams,
    ) -> Result<Value, ApiError> {
        let mut vehicle = self.companion_vehicle(&params.vehicle_id)?;
        let host = params.host.trim().to_owned();
        let user = params
            .user
            .map(|u| u.trim().to_owned())
            .filter(|u| !u.is_empty());
        companion::check_address(&host, user.as_deref()).map_err(invalid)?;
        if params.port == Some(0) {
            return Err(invalid("An SSH port is 1 to 65535"));
        }
        if vehicle
            .companions
            .iter()
            .any(|c| c.host.eq_ignore_ascii_case(&host))
        {
            return Err(invalid(format!("{host} is already one of its computers")));
        }
        let role = CompanionRole::from(params.role);
        if role == CompanionRole::Forward
            && vehicle
                .companions
                .iter()
                .any(|c| c.role == CompanionRole::Forward)
        {
            return Err(invalid(
                "It already has a computer forwarding its flight controller",
            ));
        }
        let component_id = companion::free_component_id(&vehicle.companions)
            .ok_or_else(|| invalid("A vehicle has at most four companion computers"))?;
        let key = params
            .private_key
            .map(|k| k.trim().to_owned())
            .filter(|k| !k.is_empty());
        if key
            .as_deref()
            .is_some_and(|k| !k.starts_with("-----BEGIN") || !k.contains("PRIVATE KEY-----"))
        {
            return Err(invalid(
                "That is not a private key: paste the whole file, from -----BEGIN to -----END",
            ));
        }
        let computer = CompanionComputer {
            id: format!("cc{}", &self.random.token()[..12]),
            name: params
                .name
                .map(|n| n.trim().to_owned())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| host.clone()),
            host,
            user,
            port: params.port,
            role,
            component_id,
            key: key.is_some(),
            check: None,
        };
        if let Some(key) = key {
            self.credentials
                .set(&computer.key_name(), &key)
                .map_err(|why| ApiError::new(ErrorKind::Internal, why))?;
        }
        vehicle.companions.push(computer);
        to_value(&self.save_companions(Some(connection), vehicle))
    }

    /// `computer/remove`.
    pub(crate) fn remove_computer(
        &mut self,
        connection: ConnectionId,
        ComputerParams {
            vehicle_id,
            computer_id,
        }: ComputerParams,
    ) -> Result<Value, ApiError> {
        let mut vehicle = self.companion_vehicle(&vehicle_id)?;
        let Some(at) = vehicle.companions.iter().position(|c| c.id == computer_id) else {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                format!("computer {computer_id} not found"),
            ));
        };
        let computer = vehicle.companions.remove(at);
        if computer.key {
            self.credentials
                .set(&computer.key_name(), "")
                .map_err(|why| ApiError::new(ErrorKind::Internal, why))?;
        }
        if let Some(port) = self.companions.port.as_mut() {
            port.request(CompanionOrder::Forget {
                computer_id: computer.id,
            });
        }
        to_value(&self.save_companions(Some(connection), vehicle))
    }

    /// `computer/check`: marks it checking and asks the host.
    pub(crate) fn check_computer(
        &mut self,
        connection: ConnectionId,
        ComputerCheckParams {
            vehicle_id,
            computer_id,
            password,
        }: ComputerCheckParams,
    ) -> Result<Value, ApiError> {
        let mut vehicle = self.companion_vehicle(&vehicle_id)?;
        if self.companions.port.is_none() {
            return Err(ApiError::new(
                ErrorKind::InvalidRequest,
                "Companion computers are reached over SSH from UXDuck Desktop or the uxduck daemon, not from a browser",
            ));
        }
        let at_us = self.clock.now_us();
        let Some(computer) = vehicle.companions.iter_mut().find(|c| c.id == computer_id) else {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                format!("computer {computer_id} not found"),
            ));
        };
        let pinned = computer.check.as_ref().and_then(|c| c.host_key.clone());
        computer.check = Some(CompanionCheck {
            state: CheckState::Checking,
            at_us,
            os: None,
            arch: None,
            uxduck: None,
            host_key: pinned,
            problem: None,
        });
        let order = CompanionOrder::Check {
            vehicle_id: vehicle.id.clone(),
            computer: Box::new(computer.clone()),
            password: password.filter(|p| !p.is_empty()).map(Password),
        };
        let vehicle = self.save_companions(Some(connection), vehicle);
        if let Some(port) = self.companions.port.as_mut() {
            port.request(order);
        }
        to_value(&vehicle)
    }

    /// A check's answer from the host. The pinned host key is kept through
    /// a failure, so a refused change still names the one it expected.
    pub fn companion_checked(&mut self, answer: CheckAnswer) {
        let Some(mut vehicle) = self.vehicle(&answer.vehicle_id).cloned() else {
            return;
        };
        let at_us = self.clock.now_us();
        let Some(computer) = vehicle
            .companions
            .iter_mut()
            .find(|c| c.id == answer.computer_id)
        else {
            return;
        };
        let pinned = computer.check.as_ref().and_then(|c| c.host_key.clone());
        computer.check = Some(match answer.result {
            Ok(found) => CompanionCheck {
                state: CheckState::Reached,
                at_us,
                os: found.os,
                arch: found.arch,
                uxduck: found.uxduck,
                host_key: found.host_key.or(pinned),
                problem: None,
            },
            Err(problem) => CompanionCheck {
                state: CheckState::Failed,
                at_us,
                os: None,
                arch: None,
                uxduck: None,
                host_key: pinned,
                problem: Some(problem),
            },
        });
        self.save_companions(None, vehicle);
    }
}

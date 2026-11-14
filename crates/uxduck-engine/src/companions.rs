//! A vehicle's companion computers (S296, ADR-0049 O7, O8): added by host
//! name, and checked over SSH by the host through [`CompanionPort`], which
//! answers through [`Engine::companion_checked`]. Only UXDuck Desktop and
//! the daemon have SSH; without a port, adding works and checking is refused
//! with where to do it.

use serde_json::Value;
use uxduck_contracts::dto::companion::{
    ComputerAddParams, ComputerCheckParams, ComputerNetworkParams, ComputerParams, WifiSecurity,
};
use uxduck_contracts::dto::vehicle::Vehicle as VehicleDto;
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::companion::{
    self, CheckState, CompanionCheck, CompanionClock, CompanionComputer, CompanionRole,
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
    Network {
        vehicle_id: String,
        computer: Box<CompanionComputer>,
        /// JSON on SSH stdin, wrapped so neither PSK nor credentials reach logs.
        input: Password,
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
    /// Its clock against this computer's (S356), when it could be read.
    pub clock: Option<CompanionClock>,
    pub network: Option<companion::CompanionNetwork>,
}

#[derive(Default)]
pub(crate) struct Companions {
    port: Option<Box<dyn CompanionPort>>,
    busy: std::collections::BTreeSet<String>,
    network_polled_at: std::collections::BTreeMap<String, u64>,
}

impl std::fmt::Debug for Companions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Companions")
            .field("port", &self.port.is_some())
            .field("busy", &self.busy)
            .field("network_polled_at", &self.network_polled_at)
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

fn validate_network_change(params: &ComputerNetworkParams) -> Result<(), ApiError> {
    if let Some(wifi) = &params.wifi {
        if wifi.ssid.is_empty() || wifi.ssid.len() > 32 || wifi.ssid.chars().any(char::is_control) {
            return Err(invalid(
                "A Wi-Fi name must be 1 to 32 bytes without control characters.",
            ));
        }
        if !(-999..=999).contains(&wifi.priority) {
            return Err(invalid("Wi-Fi priority must be between -999 and 999."));
        }
        let hex = wifi.password.len() == 64 && wifi.password.bytes().all(|b| b.is_ascii_hexdigit());
        if wifi.security == WifiSecurity::Open {
            if !wifi.password.is_empty() {
                return Err(invalid("An open Wi-Fi network has no password."));
            }
        } else if !((8..=63).contains(&wifi.password.len())
            || (wifi.security == WifiSecurity::WpaPsk && hex))
            || wifi.password.chars().any(char::is_control)
        {
            return Err(invalid(
                "Use an 8 to 63 byte Wi-Fi password, or a 64 digit hexadecimal WPA2 key.",
            ));
        }
    }
    if let Some(preference) = &params.preference {
        let uuid = |id: &str| {
            id.len() == 36
                && id.bytes().enumerate().all(|(i, c)| {
                    if [8, 13, 18, 23].contains(&i) {
                        c == b'-'
                    } else {
                        c.is_ascii_hexdigit()
                    }
                })
        };
        if preference.wifi_id == preference.cellular_id
            || !uuid(&preference.wifi_id)
            || !uuid(&preference.cellular_id)
        {
            return Err(invalid(
                "Choose different saved Wi-Fi and cellular connections.",
            ));
        }
    }
    if let Some(check) = &params.connectivity {
        if check.interval != 0 && !(30..=3600).contains(&check.interval) {
            return Err(invalid(
                "Check every 30 to 3600 seconds, or use zero to disable checks.",
            ));
        }
        let authority = check
            .uri
            .strip_prefix("https://")
            .or_else(|| check.uri.strip_prefix("http://"))
            .and_then(|rest| rest.split('/').next());
        if authority.is_none_or(|a| a.is_empty() || a.contains('@'))
            || check.uri.chars().any(char::is_whitespace)
        {
            return Err(invalid(
                "Use an HTTP or HTTPS health-check address without credentials.",
            ));
        }
    }
    Ok(())
}

impl Engine {
    pub(crate) fn restore_companions(&mut self) {
        let interrupted: Vec<_> = self
            .state
            .vehicles
            .values()
            .filter(|v| {
                v.companions.iter().any(|c| {
                    c.check.as_ref().is_some_and(|check| {
                        check.state == CheckState::Checking
                            || check.network.as_ref().is_some_and(|n| n.sampled_at_us > 0)
                    })
                })
            })
            .cloned()
            .collect();
        for mut vehicle in interrupted {
            for computer in &mut vehicle.companions {
                if let Some(network) = computer.check.as_mut().and_then(|c| c.network.as_mut()) {
                    network.sampled_at_us = 0;
                }
                if let Some(check) = &mut computer.check
                    && check.state == CheckState::Checking
                {
                    check.state = CheckState::Failed;
                    check.at_us = self.clock.now_us();
                    check.problem = Some("UXDuck restarted before the computer answered. Check it or refresh its networks to see what completed.".into());
                }
            }
            self.save_companions(None, vehicle);
        }
    }

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
            connectivity: None,
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
        if self.companions.busy.contains(&computer_id) {
            return Err(invalid(
                "Wait for the computer check or network update to finish before removing it.",
            ));
        }
        if vehicle.companions[at]
            .connectivity
            .as_ref()
            .is_some_and(|c| c.video != companion::CellularVideoPolicy::Allow)
        {
            return Err(invalid(
                "Choose another internet gateway or allow full cellular video before removing this one.",
            ));
        }
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

    /// `NetworkManager` settings on a reached companion, with no live Wi-Fi handover.
    pub(crate) fn network_computer(
        &mut self,
        connection: ConnectionId,
        params: &ComputerNetworkParams,
    ) -> Result<Value, ApiError> {
        if self.companions.port.is_none() {
            return Err(invalid(
                "Use UXDuck Desktop or the uxduck daemon to configure companion Wi-Fi.",
            ));
        }
        let mut vehicle = self.companion_vehicle(&params.vehicle_id)?;
        if [
            params.wifi.is_some(),
            params.connectivity.is_some(),
            params.preference.is_some(),
        ]
        .into_iter()
        .filter(|v| *v)
        .count()
            > 1
        {
            return Err(invalid("Make one network change at a time."));
        }
        if (params.wifi.is_some() || params.connectivity.is_some() || params.preference.is_some())
            && vehicle.armed
        {
            return Err(invalid("Disarm the vehicle before changing its networks."));
        }
        validate_network_change(params)?;
        let computer = vehicle
            .companions
            .iter_mut()
            .find(|c| c.id == params.computer_id)
            .ok_or_else(|| invalid("Companion computer not found."))?;
        if computer
            .check
            .as_ref()
            .and_then(|c| c.host_key.as_ref())
            .is_none()
        {
            return Err(invalid(
                "Check the computer first to verify its SSH host key.",
            ));
        }
        if !self.companions.busy.insert(computer.id.clone()) {
            return Err(invalid(
                "A check or network update is already running on this computer.",
            ));
        }
        let input = Password(
            serde_json::json!({"wifi": params.wifi, "connectivity": params.connectivity, "preference": params.preference})
                .to_string(),
        );
        let order = CompanionOrder::Network {
            vehicle_id: vehicle.id.clone(),
            computer: Box::new(computer.clone()),
            input,
        };
        if !params.background
            && let Some(check) = &mut computer.check
        {
            check.state = CheckState::Checking;
            check.problem = None;
            check.at_us = self.clock.now_us();
        }
        let vehicle = self.save_companions(Some(connection), vehicle);
        if let Some(port) = self.companions.port.as_mut() {
            port.request(order);
        }
        to_value(&vehicle)
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
        if !self.companions.busy.insert(computer.id.clone()) {
            return Err(invalid(
                "A check or network update is already running on this computer.",
            ));
        }
        let pinned = computer.check.as_ref().and_then(|c| c.host_key.clone());
        computer.check = Some(CompanionCheck {
            state: CheckState::Checking,
            at_us,
            os: None,
            arch: None,
            uxduck: None,
            host_key: pinned,
            problem: None,
            clock: None,
            network: None,
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
        self.companions.busy.remove(&answer.computer_id);
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
                network: found.network.map(|mut n| {
                    n.sampled_at_us = at_us;
                    n
                }),
                os: found.os,
                arch: found.arch,
                uxduck: found.uxduck,
                host_key: found.host_key.or(pinned),
                problem: None,
                clock: found.clock,
            },
            Err(problem) => CompanionCheck {
                state: CheckState::Failed,
                at_us,
                os: None,
                arch: None,
                uxduck: None,
                host_key: pinned,
                problem: Some(problem),
                clock: None,
                network: computer.check.as_ref().and_then(|c| c.network.clone()),
            },
        });
        self.save_companions(None, vehicle);
        self.review_recordings();
    }
}

impl Engine {
    pub(crate) fn configure_connectivity(
        &mut self,
        connection: ConnectionId,
        params: &uxduck_contracts::dto::companion::ComputerConnectivityParams,
    ) -> Result<Value, ApiError> {
        let mut vehicle = self.companion_vehicle(&params.vehicle_id)?;
        if !vehicle
            .companions
            .iter()
            .any(|c| c.id == params.computer_id)
        {
            return Err(invalid("Companion computer not found."));
        }
        for computer in &mut vehicle.companions {
            computer.connectivity =
                (computer.id == params.computer_id).then(|| companion::VehicleConnectivity {
                    video: params.video.into(),
                });
        }
        let vehicle = self.save_companions(Some(connection), vehicle);
        self.review_recordings();
        to_value(&vehicle)
    }

    /// Poll only gateways serving demanded cameras. The screen requests its
    /// own reads when open. Expiry enforcement is independent of the UI.
    pub fn connectivity_tick(&mut self, now: u64) {
        self.review_recordings();
        if self.companions.port.is_none() {
            return;
        }
        let gateways: Vec<_> = self
            .state
            .vehicles
            .values()
            .filter(|v| self.vehicle_video_wanted(&v.id))
            .flat_map(|v| {
                v.companions
                    .iter()
                    .filter(|c| c.connectivity.is_some())
                    .map(|c| (v.id.clone(), c.clone()))
            })
            .collect();
        for (vehicle_id, computer) in gateways {
            if computer
                .check
                .as_ref()
                .and_then(|c| c.host_key.as_ref())
                .is_none()
                || self.companions.busy.contains(&computer.id)
                || self
                    .companions
                    .network_polled_at
                    .get(&computer.id)
                    .is_some_and(|at| now.saturating_sub(*at) < 5_000_000)
            {
                continue;
            }
            self.companions
                .network_polled_at
                .insert(computer.id.clone(), now);
            self.companions.busy.insert(computer.id.clone());
            if let Some(port) = self.companions.port.as_mut() {
                port.request(CompanionOrder::Network {
                    vehicle_id,
                    computer: Box::new(computer),
                    input: Password("{}".into()),
                });
            }
        }
    }
}

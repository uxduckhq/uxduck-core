//! A vehicle's companion computers (S296, ADR-0049 O7, O8): added by host
//! name, and checked over SSH by the host through [`CompanionPort`], which
//! answers through [`Engine::companion_checked`]. Only UXDuck Desktop and
//! the daemon have SSH; without a port, adding works and checking is refused
//! with where to do it. Networks (ADR-0057) and cameras (ADR-0060) go through
//! the same port, one operation per computer at a time. A person's operation
//! that finds only a background network read running waits for it to answer.

use serde_json::Value;
use uxduck_contracts::dto::companion::{
    ComputerAddParams, ComputerCheckParams, ComputerNetworkParams, ComputerParams,
    ComputerRenameParams, WifiSecurity,
};
use uxduck_contracts::dto::companion_cameras::ComputerCamerasParams;
use uxduck_contracts::dto::vehicle::Vehicle as VehicleDto;
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::companion::{
    self, CheckState, CompanionCheck, CompanionClock, CompanionComputer, CompanionRole,
};
use uxduck_core::domain::companion_cameras::{self, CompanionCameras};
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
    /// Read the cameras its `MediaMTX` serves, or make one change
    /// (ADR-0060), through the exact pinned host key only.
    Cameras {
        vehicle_id: String,
        computer: Box<CompanionComputer>,
        /// JSON on SSH stdin: empty to read, else one change.
        input: String,
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
    /// The id its running UXDuck sends its readings as (S372).
    pub reports_as: Option<u8>,
    pub network: Option<companion::CompanionNetwork>,
    pub cameras: Option<CompanionCameras>,
}

#[derive(Default)]
pub(crate) struct Companions {
    port: Option<Box<dyn CompanionPort>>,
    busy: std::collections::BTreeSet<String>,
    /// Computers whose running operation is a background network read.
    reading: std::collections::BTreeSet<String>,
    /// A person's operation that arrived during a background read, sent
    /// when the read answers, and whether it changes the computer.
    waiting: std::collections::BTreeMap<String, (CompanionOrder, bool)>,
    network_polled_at: std::collections::BTreeMap<String, u64>,
}

impl std::fmt::Debug for Companions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Companions")
            .field("port", &self.port.is_some())
            .field("busy", &self.busy)
            .field("reading", &self.reading)
            .field("waiting", &self.waiting)
            .field("network_polled_at", &self.network_polled_at)
            .finish()
    }
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::InvalidValue, message)
}

fn busy() -> ApiError {
    invalid("A check, network or camera change is already running on this computer.")
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
                    check.problem = Some("UXDuck restarted before the computer answered. Check it, or read its networks or cameras again, to see what completed.".into());
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

    /// Takes the computer for one operation: `Ok(false)` to send it now, or
    /// `Ok(true)` when it is a person's and waits for the background read
    /// running there. A check or another person's operation, running or
    /// waiting, refuses it, and a background read never waits.
    fn take_computer(&mut self, computer_id: &str, background: bool) -> Result<bool, ApiError> {
        if self.companions.busy.insert(computer_id.to_owned()) {
            if background {
                self.companions.reading.insert(computer_id.to_owned());
            }
            return Ok(false);
        }
        if background
            || !self.companions.reading.contains(computer_id)
            || self.companions.waiting.contains_key(computer_id)
        {
            return Err(busy());
        }
        Ok(true)
    }

    /// Sends an order, or keeps it for when the background read it waits for
    /// answers, with whether it `changes` the computer.
    fn send_companion(
        &mut self,
        computer_id: String,
        order: CompanionOrder,
        waits: bool,
        changes: bool,
    ) {
        if waits {
            self.companions
                .waiting
                .insert(computer_id, (order, changes));
        } else if let Some(port) = self.companions.port.as_mut() {
            port.request(order);
        }
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
        // Four, because MAVLink has four onboard computer ids
        // (`companion::COMPONENT_IDS`) and each computer reports under one.
        let component_id =
            companion::free_component_id(&vehicle.companions).ok_or_else(|| {
                invalid(format!(
                    "{} has four companion computers, and MAVLink has four ids for onboard computers ({} to {}), one for each. Remove one to add another.",
                    vehicle.name,
                    companion::COMPONENT_IDS.start(),
                    companion::COMPONENT_IDS.end()
                ))
            })?;
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

    /// `computer/rename`: the name is the person's own, so it changes
    /// nothing on the computer and can change while a check runs.
    pub(crate) fn rename_computer(
        &mut self,
        connection: ConnectionId,
        ComputerRenameParams {
            vehicle_id,
            computer_id,
            name,
        }: ComputerRenameParams,
    ) -> Result<Value, ApiError> {
        let mut vehicle = self.companion_vehicle(&vehicle_id)?;
        let Some(computer) = vehicle.companions.iter_mut().find(|c| c.id == computer_id) else {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                format!("computer {computer_id} not found"),
            ));
        };
        let name = name.trim();
        if name.is_empty() {
            return Err(invalid("Give the computer a name."));
        }
        name.clone_into(&mut computer.name);
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
                "Wait for the computer's check, network or camera change to finish before removing it.",
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
        let changes =
            params.wifi.is_some() || params.connectivity.is_some() || params.preference.is_some();
        if changes && vehicle.armed {
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
        let waits = self.take_computer(&computer.id, params.background)?;
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
        self.send_companion(params.computer_id.clone(), order, waits, changes);
        to_value(&vehicle)
    }

    /// `computer/cameras` (ADR-0060): reads the cameras its `MediaMTX`
    /// serves, or makes one change, through the pinned SSH networks use (C6).
    /// A picture is checked here against the last read, and again on the
    /// computer before anything is written.
    pub(crate) fn cameras_computer(
        &mut self,
        connection: ConnectionId,
        params: &ComputerCamerasParams,
    ) -> Result<Value, ApiError> {
        if self.companions.port.is_none() {
            return Err(invalid(
                "Use UXDuck Desktop or the uxduck daemon to set up a companion's cameras.",
            ));
        }
        let mut vehicle = self.companion_vehicle(&params.vehicle_id)?;
        let changes = usize::from(params.apply.is_some())
            + usize::from(params.linger)
            + usize::from(params.watchdog.is_some());
        if changes > 1 {
            return Err(invalid("Make one camera change at a time."));
        }
        if changes > 0 && vehicle.armed {
            return Err(invalid("Disarm the vehicle before changing its cameras."));
        }
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
        if let Some(apply) = &params.apply {
            let cameras = computer.check.as_ref().and_then(|c| c.cameras.as_ref());
            let path = cameras
                .and_then(|c| c.paths.iter().find(|p| p.name == apply.path))
                .ok_or_else(|| {
                    invalid(format!(
                        "Read the cameras on {} first. UXDuck has not seen {} there.",
                        computer.name, apply.path
                    ))
                })?;
            path.adjustable().map_err(invalid)?;
            companion_cameras::check_picture(&(&apply.settings).into(), path.sensor.as_deref())
                .map_err(invalid)?;
            if !apply.despite_linger
                && cameras
                    .and_then(|c| c.linger.as_ref())
                    .is_some_and(companion_cameras::CameraLinger::at_risk)
            {
                return Err(invalid(
                    "Signing in over SSH can stop these cameras. Turn lingering on, or dismiss the warning, before changing them.",
                ));
            }
        }
        let waits = self.take_computer(&computer.id, false)?;
        let input = if changes == 0 {
            "{}".to_owned()
        } else {
            serde_json::json!({
                "apply": params.apply,
                "linger": params.linger,
                "watchdog": params.watchdog,
            })
            .to_string()
        };
        let order = CompanionOrder::Cameras {
            vehicle_id: vehicle.id.clone(),
            computer: Box::new(computer.clone()),
            input,
        };
        if let Some(check) = &mut computer.check {
            check.state = CheckState::Checking;
            check.problem = None;
            check.at_us = self.clock.now_us();
        }
        let vehicle = self.save_companions(Some(connection), vehicle);
        self.send_companion(params.computer_id.clone(), order, waits, changes > 0);
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
        let waits = self.take_computer(&computer.id, false)?;
        let last = computer.check.take();
        computer.check = Some(CompanionCheck {
            state: CheckState::Checking,
            at_us,
            os: None,
            arch: None,
            uxduck: None,
            host_key: last.as_ref().and_then(|c| c.host_key.clone()),
            problem: None,
            clock: None,
            reports_as: last.as_ref().and_then(|c| c.reports_as),
            network: last.as_ref().and_then(|c| c.network.clone()),
            cameras: last.and_then(|c| c.cameras),
        });
        let order = CompanionOrder::Check {
            vehicle_id: vehicle.id.clone(),
            computer: Box::new(computer.clone()),
            password: password.filter(|p| !p.is_empty()).map(Password),
        };
        let vehicle = self.save_companions(Some(connection), vehicle);
        self.send_companion(computer_id, order, waits, false);
        to_value(&vehicle)
    }

    /// A check's answer from the host. The pinned host key is kept through
    /// a failure, so a refused change still names the one it expected, and
    /// the last networks and cameras read are kept until read again.
    pub fn companion_checked(&mut self, answer: CheckAnswer) {
        self.companions.busy.remove(&answer.computer_id);
        self.companions.reading.remove(&answer.computer_id);
        let waiting = self.companions.waiting.remove(&answer.computer_id);
        let Some(mut vehicle) = self.vehicle(&answer.vehicle_id).cloned() else {
            return;
        };
        let at_us = self.clock.now_us();
        let stopped = vehicle.armed && waiting.as_ref().is_some_and(|(_, changes)| *changes);
        let waiting = waiting.filter(|_| !stopped).map(|(order, _)| order);
        let Some(computer) = vehicle
            .companions
            .iter_mut()
            .find(|c| c.id == answer.computer_id)
        else {
            return;
        };
        let pinned = computer.check.as_ref().and_then(|c| c.host_key.clone());
        let reports_as = computer.check.as_ref().and_then(|c| c.reports_as);
        let (network, cameras) = computer
            .check
            .as_ref()
            .map(|c| (c.network.clone(), c.cameras.clone()))
            .unwrap_or_default();
        computer.check = Some(match answer.result {
            Ok(found) => CompanionCheck {
                state: CheckState::Reached,
                at_us,
                network: found
                    .network
                    .map(|mut n| {
                        n.sampled_at_us = at_us;
                        n
                    })
                    .or(network),
                os: found.os,
                arch: found.arch,
                uxduck: found.uxduck,
                cameras: found.cameras.or(cameras),
                host_key: found.host_key.or(pinned),
                problem: None,
                clock: found.clock,
                reports_as: found.reports_as.or(reports_as),
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
                reports_as,
                network,
                cameras,
            },
        });
        if let Some(check) = &mut computer.check {
            if waiting.is_some() {
                check.state = CheckState::Checking;
                check.problem = None;
            } else if stopped {
                check.state = CheckState::Failed;
                check.problem = Some(
                    "The vehicle armed before the change started, so nothing changed. Disarm it to try again."
                        .into(),
                );
            }
        }
        companion::take_reported_id(&mut vehicle.companions, &answer.computer_id);
        self.save_companions(None, vehicle);
        self.review_recordings();
        if let Some(order) = waiting {
            self.companions.busy.insert(answer.computer_id);
            if let Some(port) = self.companions.port.as_mut() {
                port.request(order);
            }
        }
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
            self.companions.reading.insert(computer.id.clone());
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

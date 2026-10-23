//! Links and vehicles: add, change, turn off and remove links; list, get,
//! save and dismiss vehicles (ADR-0004).

use serde_json::{Value, json};
use uxduck_contracts::dto::initialize::RunsIn;
use uxduck_contracts::dto::link::{
    IdParams, Link as LinkDto, LinkAddParams, LinkListResult, LinkUpdateParams, SerialListResult,
    SerialPort,
};
use uxduck_contracts::dto::vehicle::{
    Vehicle as VehicleDto, VehicleBookmarksParams, VehicleLabelParams, VehicleListResult,
    VehicleSaveParams, VehicleSizeParams,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::link::{Link, LinkKind, LinkStatus};
use uxduck_core::events::Change;

use super::handlers::to_value;
use crate::{ConnectionId, Engine};

pub fn handle(
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
        ClientRequest::LinkList(_) => {
            require(Capability::Read)?;
            to_value(&LinkListResult {
                links: links(engine),
            })
        }
        ClientRequest::VehicleList(_) => {
            require(Capability::Read)?;
            to_value(&VehicleListResult {
                vehicles: vehicles(engine)
                    .into_iter()
                    .filter(|v| !v.dismissed)
                    .collect(),
            })
        }
        ClientRequest::LinkEnable(IdParams { id }) => {
            require(Capability::Config)?;
            enable_link(engine, connection, &id)
        }
        ClientRequest::LinkDisable(IdParams { id }) => {
            require(Capability::Config)?;
            disable_link(engine, connection, &id)
        }
        ClientRequest::LinkUpdate(params) => {
            require(Capability::Config)?;
            update_link(engine, connection, params)
        }
        ClientRequest::SerialList(_) => {
            require(Capability::Config)?;
            to_value(&serial_ports(engine))
        }
        ClientRequest::TelemetrySubscribe(_) => {
            require(Capability::Read)?;
            to_value(&engine.subscribe_telemetry(connection))
        }
        ClientRequest::TelemetryGet(IdParams { id }) => {
            require(Capability::Read)?;
            telemetry_of(engine, &id)
        }
        ClientRequest::TelemetryWindow(params) => {
            require(Capability::Read)?;
            to_value(&engine.telemetry_window(&params)?)
        }
        ClientRequest::VehicleGet(IdParams { id }) => {
            require(Capability::Read)?;
            to_value(&live_vehicle(engine, found(engine, &id)?))
        }
        ClientRequest::VehicleSave(params) => {
            require(Capability::Config)?;
            save_vehicle(engine, connection, params)
        }
        ClientRequest::VehicleLabel(params) => {
            require(Capability::Config)?;
            label_vehicles(engine, connection, params)
        }
        ClientRequest::VehicleSize(params) => {
            require(Capability::Config)?;
            size_vehicle(engine, connection, params)
        }
        ClientRequest::VehicleBookmarks(params) => {
            require(Capability::Config)?;
            bookmark_vehicle(engine, connection, params)
        }
        ClientRequest::VehicleDismiss(IdParams { id }) => {
            require(Capability::Config)?;
            dismiss_vehicle(engine, connection, &id)
        }
        ClientRequest::VehicleDelete(IdParams { id }) => {
            require(Capability::Config)?;
            delete_vehicle(engine, connection, &id)
        }
        ClientRequest::LinkAdd(params) => {
            require(Capability::Config)?;
            add_link(engine, connection, params)
        }
        ClientRequest::LinkRemove(IdParams { id }) => {
            require(Capability::Config)?;
            remove_link_asked(engine, connection, &id)
        }
        _ => Err(ApiError::new(
            ErrorKind::Internal,
            "not a vehicle or link method",
        )),
    }
}

/// Vehicles, heard ones first, then by name.
pub fn vehicles(engine: &Engine) -> Vec<VehicleDto> {
    let mut vehicles: Vec<VehicleDto> = engine
        .state
        .vehicles
        .values()
        .map(|vehicle| live_vehicle(engine, vehicle))
        .collect();
    vehicles.sort_by(|a, b| (a.presence as u8, &a.name).cmp(&(b.presence as u8, &b.name)));
    vehicles
}

/// A direct read uses the host's live heartbeat table, keeping unchanged
/// heartbeats out of the durable journal and entity version counter.
fn live_vehicle(engine: &Engine, vehicle: &uxduck_core::domain::vehicle::Vehicle) -> VehicleDto {
    let mut result = VehicleDto::from(vehicle);
    let ids: Vec<String> = std::iter::once(vehicle.id.clone())
        .chain(vehicle.aliases.iter().cloned())
        .collect();
    if let Some(at_us) = engine.system.vehicle_heartbeat_us(&ids) {
        result.last_heartbeat_us = result.last_heartbeat_us.max(at_us);
        result.heartbeat_age_ms = Some(
            engine
                .clock
                .now_us()
                .saturating_sub(result.last_heartbeat_us)
                / 1000,
        );
    }
    result
}

pub fn links(engine: &Engine) -> Vec<LinkDto> {
    let mut links: Vec<LinkDto> = engine.state.links.values().map(LinkDto::from).collect();
    links.sort_by_key(|link| link.created_at_us);
    links
}

/// A vehicle's telemetry, empty until it has sent any.
fn telemetry_of(engine: &Engine, id: &str) -> Result<Value, ApiError> {
    let vehicle = found(engine, id)?;
    let telemetry = engine.telemetry(&vehicle.id).cloned().unwrap_or_else(|| {
        uxduck_core::domain::telemetry::VehicleTelemetry {
            vehicle_id: vehicle.id.clone(),
            ..Default::default()
        }
    });
    to_value(&uxduck_contracts::dto::telemetry::VehicleTelemetry::from(
        &telemetry,
    ))
}

fn found<'e>(
    engine: &'e Engine,
    id: &str,
) -> Result<&'e uxduck_core::domain::vehicle::Vehicle, ApiError> {
    engine
        .vehicle(id)
        .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no vehicle {id}")))
}

fn add_link(
    engine: &mut Engine,
    connection: ConnectionId,
    params: LinkAddParams,
) -> Result<Value, ApiError> {
    let actor = engine.actor(connection);
    to_value(&create_link(engine, actor, params)?)
}

/// Checks and creates a link, and opens it.
pub(crate) fn create_link(
    engine: &mut Engine,
    actor: uxduck_core::events::Actor,
    mut params: LinkAddParams,
) -> Result<LinkDto, ApiError> {
    let from_link = params.from_link.take();
    create_link_from(engine, actor, params, from_link)
}

/// [`create_link`], for a forward that carries only `from_link`'s traffic
/// and goes with it (S102).
pub(crate) fn create_link_from(
    engine: &mut Engine,
    actor: uxduck_core::events::Actor,
    params: LinkAddParams,
    from_link: Option<String>,
) -> Result<LinkDto, ApiError> {
    let kind: LinkKind = params.kind.into();
    if let Some(source) = &from_link {
        if !kind.is_forward() || !params.vehicle_filter.is_empty() {
            return Err(invalid(
                "fromLink requires a UDP forward without a vehicle filter",
            ));
        }
        let Some(source) = engine.state.links.get(source) else {
            return Err(invalid("the simulation to forward no longer exists"));
        };
        if !source.kind.is_simulator() {
            return Err(invalid("fromLink must name a simulated vehicle's link"));
        }
    }
    let default_name = check_kind(engine, &kind, None)?;
    let kind = crate::sim::assign_instance(engine, kind);
    let name = params
        .name
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or(default_name);
    let kind_is_forward = kind.is_forward();
    let vehicle_filter: Vec<String> = if kind_is_forward {
        params
            .vehicle_filter
            .into_iter()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
            .collect()
    } else {
        Vec::new()
    };
    crate::forwarding::check(engine, &kind, from_link.as_deref(), &vehicle_filter, None)?;
    let link = Link {
        id: format!("li{}", &engine.random.token()[..16]),
        name,
        kind,
        policy: params.policy.map(Into::into).unwrap_or_default(),
        topology: params.topology.map(Into::into).unwrap_or_default(),
        status: LinkStatus::Configured,
        status_detail: None,
        vehicle_filter,
        from_link: from_link.filter(|_| kind_is_forward),
        keep_alive: params.keep_alive,
        launch: None,
        paused: false,
        created_at_us: engine.clock.now_us(),
        version: 1,
    };
    engine.commit_as(actor, Change::LinkUpserted { link: link.clone() });
    engine.links.open(&link);
    engine.begin_launch(&link.id);
    if link.kind.is_forward() {
        engine.suggest_start_at_login();
    }
    let link = engine.state.links.get(&link.id).unwrap_or(&link);
    Ok(LinkDto::from(link))
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::InvalidValue, message)
}

fn is_ip(host: &str) -> bool {
    host.parse::<std::net::IpAddr>().is_ok()
}

/// What a link of this kind is called unless named: a listener by its port
/// ("UDP 15010", "TCP 5760"), a client by its address ("TCP
/// 127.0.0.1:5760", "UDP 192.168.4.1:14550"), "Serial ttyACM0", "Forward to
/// 127.0.0.1:14550". A simulator's name comes from the simulation manager.
fn default_name(kind: &LinkKind) -> Option<String> {
    match kind {
        LinkKind::UdpListen { port, .. } => Some(format!("UDP {port}")),
        LinkKind::Serial { device, .. } => {
            let short = device.rsplit(['/', '\\']).next().unwrap_or(device);
            Some(format!("Serial {short}"))
        }
        LinkKind::TcpClient { host, port } => Some(format!("TCP {host}:{port}")),
        LinkKind::TcpServer { port, .. } => Some(format!("TCP {port}")),
        LinkKind::UdpClient { host, port } => Some(format!("UDP {host}:{port}")),
        LinkKind::UdpForward { host, port } => Some(format!("Forward to {host}:{port}")),
        LinkKind::Simulator { .. } => None,
    }
}

/// Checks a link's kind against the others and the host (ADR-0004 port
/// ownership and routing rules), leaving out the link being changed
/// (`except`). Returns the default name.
pub(crate) fn check_kind(
    engine: &Engine,
    kind: &LinkKind,
    except: Option<&str>,
) -> Result<String, ApiError> {
    if engine.config.capabilities.runs_in == RunsIn::Browser && !kind.is_simulator() {
        return Err(invalid(
            "UXDuck Web runs only simulated vehicles. Real ones connect through UXDuck Desktop",
        ));
    }
    let links: Vec<&Link> = engine
        .state
        .links
        .values()
        .filter(|l| except != Some(l.id.as_str()))
        .collect();
    let listens_on = |port: u16| {
        links
            .iter()
            .any(|l| matches!(&l.kind, LinkKind::UdpListen { port: p, .. } if *p == port))
    };
    match kind {
        LinkKind::UdpListen { host, port } => {
            if *port == 0 || !is_ip(host) {
                return Err(invalid(
                    "a UDP link needs an IP address and a port from 1 to 65535",
                ));
            }
            if listens_on(*port) {
                return Err(invalid(format!(
                    "another link already listens on port {port}"
                )));
            }
            let sent_here = links.iter().any(|l| {
                matches!(&l.kind, LinkKind::UdpForward { host: to, port: p } | LinkKind::UdpClient { host: to, port: p }
                    if p == port && engine.system.is_local_address(to))
            });
            if sent_here {
                return Err(invalid(format!(
                    "another link already sends to port {port} on this machine. Listening there would loop"
                )));
            }
            engine
                .system
                .check_udp_bind(host, *port)
                .map_err(|detail| ApiError::new(ErrorKind::PortInUse, detail))?;
            Ok(default_name(kind).unwrap_or_default())
        }
        LinkKind::Serial { device, baud, usb } => {
            if device.trim().is_empty() || *baud == 0 {
                return Err(invalid("a serial link needs a device and a baud rate"));
            }
            let taken = links.iter().any(|l| match &l.kind {
                LinkKind::Serial {
                    device: other,
                    usb: other_usb,
                    ..
                } => other == device || (usb.is_some() && other_usb == usb),
                _ => false,
            });
            if taken {
                return Err(invalid(format!("another link already uses {device}")));
            }
            Ok(default_name(kind).unwrap_or_default())
        }
        LinkKind::TcpClient { host, port } => {
            if host.trim().is_empty() || *port == 0 {
                return Err(invalid(
                    "a TCP link needs a host and a port from 1 to 65535",
                ));
            }
            Ok(default_name(kind).unwrap_or_default())
        }
        LinkKind::TcpServer { host, port } => check_tcp_server(&links, kind, host, *port),
        LinkKind::UdpClient { host, port } => {
            check_udp_client(engine, &links, kind, host, *port, listens_on(*port))
        }
        LinkKind::UdpForward { host, port } => {
            if *port == 0 || !(is_ip(host) || host == "localhost") {
                return Err(invalid(
                    "a forward needs an IP address and a port from 1 to 65535",
                ));
            }
            // Never forward into our own listener: every frame would loop.
            if listens_on(*port) && engine.system.is_local_address(host) {
                return Err(invalid(format!(
                    "{host}:{port} is one of this daemon's own listeners. Forwarding there would loop"
                )));
            }
            Ok(default_name(kind).unwrap_or_default())
        }
        LinkKind::Simulator { .. } => crate::sim::check(engine, &links, kind),
    }
}

/// A TCP listener: an IP address, and one link per port.
fn check_tcp_server(
    links: &[&Link],
    kind: &LinkKind,
    host: &str,
    port: u16,
) -> Result<String, ApiError> {
    if port == 0 || !is_ip(host) {
        return Err(invalid(
            "a TCP server needs an IP address and a port from 1 to 65535",
        ));
    }
    let taken = links
        .iter()
        .any(|l| matches!(&l.kind, LinkKind::TcpServer { port: p, .. } if *p == port));
    if taken {
        return Err(invalid(format!(
            "another link already listens on TCP port {port}"
        )));
    }
    Ok(default_name(kind).unwrap_or_default())
}

/// A UDP client: never into one of our own listeners, where every frame
/// would loop, and one link per server.
fn check_udp_client(
    engine: &Engine,
    links: &[&Link],
    kind: &LinkKind,
    host: &str,
    port: u16,
    listened_on: bool,
) -> Result<String, ApiError> {
    if host.trim().is_empty() || port == 0 {
        return Err(invalid(
            "a UDP client needs a host and a port from 1 to 65535",
        ));
    }
    if listened_on && engine.system.is_local_address(host) {
        return Err(invalid(format!(
            "{host}:{port} is one of this daemon's own listeners. Connecting there would loop"
        )));
    }
    let duplicate = links.iter().any(
        |l| matches!(&l.kind, LinkKind::UdpClient { host: h, port: p } if h == host && *p == port),
    );
    if duplicate {
        return Err(invalid(format!(
            "another link already connects to {host}:{port}"
        )));
    }
    Ok(default_name(kind).unwrap_or_default())
}

/// Turns a link on again after the user, or loop detection, turned it off.
fn enable_link(engine: &mut Engine, connection: ConnectionId, id: &str) -> Result<Value, ApiError> {
    let Some(mut link) = engine.state.links.get(id).cloned() else {
        return Err(ApiError::new(ErrorKind::NotFound, format!("no link {id}")));
    };
    // A simulation from the last run starts as "Start again" starts it.
    if link.status == LinkStatus::Stopped {
        return crate::sim::restart(engine, connection, id);
    }
    if link.status.is_off() {
        link.status = LinkStatus::Configured;
        link.status_detail = None;
        link.version += 1;
        engine.commit(connection, Change::LinkUpserted { link: link.clone() });
        engine.links.open(&link);
    }
    engine.resolve(&format!("link-loop:{id}"));
    to_value(&LinkDto::from(&link))
}

/// `link/disable`: closes the link and keeps it, with its settings, until
/// `link/enable`. Its vehicles go offline, as a removed link's do.
fn disable_link(
    engine: &mut Engine,
    connection: ConnectionId,
    id: &str,
) -> Result<Value, ApiError> {
    let Some(mut link) = engine.state.links.get(id).cloned() else {
        return Err(ApiError::new(ErrorKind::NotFound, format!("no link {id}")));
    };
    if link.kind.is_simulator() {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "Stop the simulation instead. A simulated vehicle can't be turned off.",
        ));
    }
    if link.status != LinkStatus::Disabled {
        engine.links.close(id);
        link.status = LinkStatus::Disabled;
        link.status_detail = None;
        link.version += 1;
        engine.commit(connection, Change::LinkUpserted { link: link.clone() });
        vehicles_offline(engine, connection, id);
        engine.resolve(&format!("link-loop:{id}"));
        engine.refresh_wake_lock();
    }
    to_value(&LinkDto::from(&link))
}

/// `link/update`: changes a link in place. A new kind is checked as a new
/// link's is, leaving this link out; the link keeps its id, so its vehicles
/// stay its vehicles. A new kind reopens it (its vehicles go offline until
/// heard again); a new policy or filter changes the open link's role; a
/// name, a topology or keep-alive are only saved.
fn update_link(
    engine: &mut Engine,
    connection: ConnectionId,
    params: LinkUpdateParams,
) -> Result<Value, ApiError> {
    let Some(before) = engine.state.links.get(&params.id).cloned() else {
        return Err(ApiError::new(
            ErrorKind::NotFound,
            format!("no link {}", params.id),
        ));
    };
    let more_than_name = params.kind.is_some()
        || params.policy.is_some()
        || params.topology.is_some()
        || params.vehicle_filter.is_some()
        || params.keep_alive.is_some();
    if before.kind.is_simulator() && more_than_name {
        return Err(invalid(
            "a simulated vehicle's link takes only a new name. How it runs is set when the simulation starts",
        ));
    }
    let mut link = before.clone();
    if let Some(kind) = params.kind {
        let kind: LinkKind = kind.into();
        if kind.is_simulator() {
            return Err(invalid(
                "a link cannot become a simulated vehicle. Start one instead",
            ));
        }
        if before.from_link.is_some() && !kind.is_forward() {
            return Err(invalid("a simulation's forward must stay a UDP forward"));
        }
        if kind != before.kind {
            let name = check_kind(engine, &kind, Some(&before.id))?;
            // A link still called what its kind made it follows the new kind.
            if Some(&link.name) == default_name(&before.kind).as_ref() {
                link.name = name;
            }
            link.kind = kind;
        }
    }
    if let Some(name) = params.name {
        let name = name.trim();
        if name.chars().count() > 64 {
            return Err(invalid("a link name has at most 64 characters"));
        }
        if !name.is_empty() {
            name.clone_into(&mut link.name);
        } else if let Some(name) = default_name(&link.kind) {
            link.name = name;
        }
    }
    if let Some(policy) = params.policy {
        link.policy = policy.into();
    }
    if let Some(topology) = params.topology {
        link.topology = topology.into();
    }
    if let Some(filter) = params.vehicle_filter {
        if link.from_link.is_some() && !filter.is_empty() {
            return Err(invalid(
                "a simulation's forward cannot have a vehicle filter",
            ));
        }
        link.vehicle_filter = if link.kind.is_forward() {
            filter
                .into_iter()
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
                .collect()
        } else {
            Vec::new()
        };
    } else if !link.kind.is_forward() {
        link.vehicle_filter.clear();
    }
    if let Some(keep_alive) = params.keep_alive {
        link.keep_alive = keep_alive;
    }
    if link == before {
        return to_value(&LinkDto::from(&before));
    }
    crate::forwarding::check_update(engine, &before, &link)?;
    let moved = link.kind != before.kind;
    let reopens = !link.status.is_off()
        && (moved || link.policy != before.policy || link.vehicle_filter != before.vehicle_filter);
    if reopens && moved {
        link.status = LinkStatus::Configured;
        link.status_detail = None;
    }
    link.version += 1;
    engine.commit(connection, Change::LinkUpserted { link: link.clone() });
    if reopens {
        engine.links.update(&link);
        if moved {
            vehicles_offline(engine, connection, &link.id);
        }
    }
    if link.kind.is_forward() && !before.kind.is_forward() {
        engine.suggest_start_at_login();
    }
    let link = engine.state.links.get(&link.id).unwrap_or(&link);
    to_value(&LinkDto::from(link))
}

/// The vehicles heard on a link are offline once it closes: nothing on it
/// reports them again until it opens. They are remembered, name and all.
pub(crate) fn vehicles_offline(engine: &mut Engine, connection: ConnectionId, link_id: &str) {
    let on_link: Vec<_> = engine
        .state
        .vehicles
        .values()
        .filter(|v| {
            v.link_id == link_id && v.presence != uxduck_core::domain::vehicle::Presence::Offline
        })
        .cloned()
        .collect();
    for mut vehicle in on_link {
        vehicle.presence = uxduck_core::domain::vehicle::Presence::Offline;
        vehicle.version += 1;
        engine.commit(connection, Change::VehicleUpserted { vehicle });
    }
}

fn serial_ports(engine: &Engine) -> SerialListResult {
    SerialListResult {
        ports: engine
            .system
            .serial_ports()
            .into_iter()
            .map(|p| SerialPort {
                device: p.device,
                usb: p.usb.as_ref().map(Into::into),
                description: p.description,
            })
            .collect(),
    }
}

/// Closes the link; the live records of unsaved vehicles on it go, saved
/// vehicles stay, offline (ADR-0004: removing a link never deletes a profile).
/// Hides a vehicle until it is saved or the daemon restarts.
fn dismiss_vehicle(
    engine: &mut Engine,
    connection: ConnectionId,
    id: &str,
) -> Result<Value, ApiError> {
    let mut vehicle = found(engine, id)?.clone();
    vehicle.dismissed = true;
    vehicle.version += 1;
    let result = VehicleDto::from(&vehicle);
    engine.commit(connection, Change::VehicleUpserted { vehicle });
    to_value(&result)
}

/// Forgets an offline vehicle. A vehicle still heard would vanish while it
/// flies: its link reports only changes, so nothing would bring it back.
fn delete_vehicle(
    engine: &mut Engine,
    connection: ConnectionId,
    id: &str,
) -> Result<Value, ApiError> {
    let vehicle = found(engine, id)?;
    if vehicle.presence != uxduck_core::domain::vehicle::Presence::Offline {
        return Err(ApiError::new(
            ErrorKind::Busy,
            format!(
                "{} is still connected. You can delete it once it goes offline.",
                vehicle.name
            ),
        ));
    }
    let id = vehicle.id.clone();
    engine.commit(connection, Change::VehicleRemoved { id });
    Ok(json!({}))
}

/// `vehicle/save`: keeps it, renamed or moved in or out of the account (a
/// simulated vehicle is only renamed);
/// a new name goes in its history (S69).
fn save_vehicle(
    engine: &mut Engine,
    connection: ConnectionId,
    VehicleSaveParams {
        id,
        name,
        local_only,
    }: VehicleSaveParams,
) -> Result<Value, ApiError> {
    let before = found(engine, &id)?.clone();
    let mut vehicle = before.clone();
    if let Some(name) = name {
        let name = name.trim();
        if name.chars().count() > 64 {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "a vehicle name has up to 64 characters",
            ));
        }
        // An empty name puts back one nobody chose: a callsign
        // (maintainer, 2026-10-07; callsigns 2026-10-10).
        vehicle.name = if name.is_empty() {
            engine.free_callsign(Some(&vehicle.id))
        } else {
            name.to_owned()
        };
    }
    if let Some(local_only) = local_only {
        vehicle.local_only = local_only;
    }
    // A simulated vehicle is only renamed: kept, it would outlive its
    // simulation, which ends only with Stop (maintainer, 2026-10-07).
    let simulated = engine
        .state
        .links
        .get(&vehicle.link_id)
        .is_some_and(|link| link.kind.is_simulator());
    if !simulated {
        vehicle.saved = true;
    }
    vehicle.dismissed = false;
    vehicle.version += 1;
    let result = VehicleDto::from(&vehicle);
    let after = vehicle.clone();
    engine.commit(connection, Change::VehicleUpserted { vehicle });
    engine.record_vehicle_change(Some(&before), &after);
    to_value(&result)
}

/// The longest label, in characters, and the most on one vehicle (S106).
const MAX_LABEL_CHARS: usize = 40;
const MAX_LABELS: usize = 20;

/// `vehicle/label`: labels on and off several vehicles (S106). A label
/// matches in any case; a new one takes the spelling already in use on
/// another vehicle, unless this call takes that spelling off, which is how
/// "survey" becomes "Survey". Every vehicle is checked before any changes,
/// so a bad id or a full vehicle changes nothing. Not in the history: a
/// label is the person's organisation, not something that happened to it.
fn label_vehicles(
    engine: &mut Engine,
    connection: ConnectionId,
    VehicleLabelParams { ids, add, remove }: VehicleLabelParams,
) -> Result<Value, ApiError> {
    let clean = |label: &String| {
        let label = label.trim();
        if label.is_empty() || label.chars().count() > MAX_LABEL_CHARS {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                format!("a label has 1 to {MAX_LABEL_CHARS} characters"),
            ));
        }
        Ok(label.to_owned())
    };
    let add = add.iter().map(clean).collect::<Result<Vec<_>, _>>()?;
    let remove: Vec<String> = remove.iter().map(|l| l.trim().to_lowercase()).collect();
    let mut targets = Vec::new();
    for id in &ids {
        let vehicle = found(engine, id)?;
        if !targets
            .iter()
            .any(|t: &uxduck_core::domain::vehicle::Vehicle| t.id == vehicle.id)
        {
            targets.push(vehicle.clone());
        }
    }
    // The spelling in use elsewhere, unless this call takes it off.
    let add: Vec<String> = add
        .into_iter()
        .map(|label| {
            let key = label.to_lowercase();
            if remove.contains(&key) {
                return label;
            }
            engine
                .state
                .vehicles
                .values()
                .flat_map(|v| &v.labels)
                .find(|l| l.to_lowercase() == key)
                .cloned()
                .unwrap_or(label)
        })
        .collect();
    let mut changed = Vec::new();
    for mut vehicle in targets {
        let mut labels: Vec<String> = vehicle
            .labels
            .iter()
            .filter(|l| !remove.contains(&l.to_lowercase()))
            .cloned()
            .collect();
        for label in &add {
            if !labels
                .iter()
                .any(|l| l.to_lowercase() == label.to_lowercase())
            {
                labels.push(label.clone());
            }
        }
        labels.sort_by_key(|l| l.to_lowercase());
        if labels.len() > MAX_LABELS {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                format!("{} would have more than {MAX_LABELS} labels", vehicle.name),
            ));
        }
        if labels != vehicle.labels {
            vehicle.labels = labels;
            changed.push(vehicle);
        }
    }
    let mut result = Vec::new();
    for mut vehicle in changed {
        vehicle.version += 1;
        result.push(VehicleDto::from(&vehicle));
        engine.commit(connection, Change::VehicleUpserted { vehicle });
    }
    to_value(&VehicleListResult { vehicles: result })
}

/// The largest size a vehicle may be given, 1 km, in centimetres.
const MAX_SIZE_CM: u32 = 100_000;

/// `vehicle/size`: its length, or wingspan, for the 3D map (maintainer,
/// 2026-10-13), or none, for the size of the one its autopilot's simulator
/// flies. Neither saves the vehicle nor goes in its history: it is how the
/// person sees it, as a label is.
fn size_vehicle(
    engine: &mut Engine,
    connection: ConnectionId,
    VehicleSizeParams { id, size_cm }: VehicleSizeParams,
) -> Result<Value, ApiError> {
    if size_cm.is_some_and(|cm| cm == 0 || cm > MAX_SIZE_CM) {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "a size is 1 cm to 1 km",
        ));
    }
    let mut vehicle = found(engine, &id)?.clone();
    if vehicle.size_cm != size_cm {
        vehicle.size_cm = size_cm;
        vehicle.version += 1;
        engine.commit(
            connection,
            Change::VehicleUpserted {
                vehicle: vehicle.clone(),
            },
        );
    }
    to_value(&VehicleDto::from(&vehicle))
}

/// The longest name for a vehicle's web page, and the longest address. As
/// many pages as the person likes.
const MAX_BOOKMARK_NAME_CHARS: usize = 40;
const MAX_BOOKMARK_URL_CHARS: usize = 2048;

/// `vehicle/bookmarks`: the web pages kept with a vehicle, such as its
/// radio's settings page (maintainer, 2026-10-15), all of them in order.
/// Only `http` and `https` addresses, so a page in the interface never
/// opens a `javascript:` or `file:` one. Like its size, it neither saves the
/// vehicle nor goes in its history.
fn bookmark_vehicle(
    engine: &mut Engine,
    connection: ConnectionId,
    VehicleBookmarksParams { id, bookmarks }: VehicleBookmarksParams,
) -> Result<Value, ApiError> {
    let invalid = |message: String| ApiError::new(ErrorKind::InvalidValue, message);
    let bookmarks = bookmarks
        .iter()
        .map(|b| {
            let name = b.name.trim();
            if name.is_empty() || name.chars().count() > MAX_BOOKMARK_NAME_CHARS {
                return Err(invalid(format!(
                    "a bookmark's name has 1 to {MAX_BOOKMARK_NAME_CHARS} characters"
                )));
            }
            let url = b.url.trim();
            if !web_address(url) {
                return Err(invalid(format!(
                    "{url:?} isn't a web address. Start it with http:// or https://"
                )));
            }
            Ok(uxduck_core::domain::vehicle::Bookmark {
                name: name.to_owned(),
                url: url.to_owned(),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut vehicle = found(engine, &id)?.clone();
    if vehicle.bookmarks != bookmarks {
        vehicle.bookmarks = bookmarks;
        vehicle.version += 1;
        engine.commit(
            connection,
            Change::VehicleUpserted {
                vehicle: vehicle.clone(),
            },
        );
    }
    to_value(&VehicleDto::from(&vehicle))
}

/// An `http` or `https` address with a host, without spaces or control
/// characters, short enough to keep.
fn web_address(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let Some(rest) = lower
        .strip_prefix("http://")
        .or_else(|| lower.strip_prefix("https://"))
    else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !host.is_empty()
        && url.chars().count() <= MAX_BOOKMARK_URL_CHARS
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// `link/remove`: any link but a simulation's, which has one way to end so
/// there is no telling the two apart: `sim/stop`.
fn remove_link_asked(
    engine: &mut Engine,
    connection: ConnectionId,
    id: &str,
) -> Result<Value, ApiError> {
    if engine
        .state
        .links
        .get(id)
        .is_some_and(|l| l.kind.is_simulator())
    {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "This connection closes by itself when you stop the simulation.",
        ));
    }
    remove_link(engine, connection, id)
}

pub(crate) fn remove_link(
    engine: &mut Engine,
    connection: ConnectionId,
    id: &str,
) -> Result<Value, ApiError> {
    if !engine.state.links.contains_key(id) {
        return Err(ApiError::new(ErrorKind::NotFound, format!("no link {id}")));
    }
    engine.links.close(id);
    engine.forget_flights_on(id);
    let on_link: Vec<_> = engine
        .state
        .vehicles
        .values()
        .filter(|v| v.link_id == id)
        .cloned()
        .collect();
    // A vehicle is remembered, name and all, saved or not; a simulated one
    // goes with its simulation.
    let simulated = engine.state.links[id].kind.is_simulator();
    for mut vehicle in on_link {
        if !simulated || vehicle.saved {
            // A stopped simulator cannot remain armed. Its saved profile
            // outlives the simulation link, so preserve no armed evidence
            // that could be mistaken for disconnected hardware later.
            if simulated {
                vehicle.armed = false;
            }
            vehicle.presence = uxduck_core::domain::vehicle::Presence::Offline;
            vehicle.version += 1;
            engine.commit(connection, Change::VehicleUpserted { vehicle });
        } else {
            for vehicle_id in std::iter::once(&vehicle.id).chain(&vehicle.aliases) {
                let vehicle_id = vehicle_id.clone();
                engine.commit(connection, Change::HistoryCleared { vehicle_id });
            }
            engine.commit(connection, Change::VehicleRemoved { id: vehicle.id });
        }
    }
    engine.commit(connection, Change::LinkRemoved { id: id.to_owned() });
    engine.removed_links.insert(id.to_owned());
    engine.drop_launch(id);
    engine.resolve(&format!("link-loop:{id}"));
    // A simulated vehicle's forward goes with it (S102).
    let forwards: Vec<String> = engine
        .state
        .links
        .values()
        .filter(|l| l.from_link.as_deref() == Some(id))
        .map(|l| l.id.clone())
        .collect();
    for forward in forwards {
        remove_link(engine, connection, &forward)?;
    }
    engine.refresh_wake_lock();
    Ok(json!({}))
}

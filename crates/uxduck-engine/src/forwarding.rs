//! Forward destinations may be reused by disjoint traffic scopes.
use crate::Engine;
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::link::{Link, LinkKind};
use uxduck_core::domain::vehicle::Vehicle;

#[derive(Clone, Copy)]
enum Scope<'a> {
    All,
    Vehicles(&'a [String]),
    Source(&'a str),
    /// A simulation that has not been created yet is a distinct source link.
    NewSimulation,
}
fn scope<'a>(source: Option<&'a str>, vehicles: &'a [String]) -> Scope<'a> {
    source.map_or_else(
        || {
            if vehicles.is_empty() {
                Scope::All
            } else {
                Scope::Vehicles(vehicles)
            }
        },
        Scope::Source,
    )
}
fn host_key(host: &str) -> String {
    if host == "localhost" {
        "127.0.0.1".into()
    } else {
        host.parse::<std::net::IpAddr>()
            .map_or_else(|_| host.to_owned(), |ip| ip.to_string())
    }
}
fn same_destination(a: &LinkKind, b: &LinkKind) -> bool {
    matches!((a, b), (LinkKind::UdpForward { host: ah, port: ap }, LinkKind::UdpForward { host: bh, port: bp }) if ap == bp && host_key(ah) == host_key(bh))
}
fn overlap(engine: &Engine, a: Scope<'_>, b: Scope<'_>) -> bool {
    match (a, b) {
        (Scope::Source(a), Scope::Source(b)) => a == b,
        (Scope::NewSimulation, Scope::Source(_) | Scope::NewSimulation)
        | (Scope::Source(_), Scope::NewSimulation) => false,
        (Scope::Vehicles(a), Scope::Vehicles(b)) => a.iter().any(|a| {
            b.iter().any(|b| {
                engine.vehicle(a).map_or(a.as_str(), |v| v.id.as_str())
                    == engine.vehicle(b).map_or(b.as_str(), |v| v.id.as_str())
            })
        }),
        // A vehicle filter can move onto a source link later. Keep mixed
        // source/filter and all-vehicle scopes on one shared forward.
        _ => true,
    }
}
fn carries(scope: Scope<'_>, vehicle: &Vehicle) -> bool {
    match scope {
        Scope::All => true,
        Scope::Source(link) => vehicle.link_id == link,
        Scope::Vehicles(ids) => ids
            .iter()
            .any(|id| id == &vehicle.id || vehicle.aliases.contains(id)),
        Scope::NewSimulation => false,
    }
}
fn validate(
    engine: &Engine,
    kind: &LinkKind,
    desired: Scope<'_>,
    except: Option<&str>,
) -> Result<(), ApiError> {
    let LinkKind::UdpForward { host, port } = kind else {
        return Ok(());
    };
    let shared = engine
        .state
        .links
        .values()
        .filter(|link| except != Some(link.id.as_str()) && same_destination(kind, &link.kind))
        .collect::<Vec<&Link>>();
    for other in &shared {
        let existing = scope(other.from_link.as_deref(), &other.vehicle_filter);
        if overlap(engine, desired, existing) {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                format!(
                    "a forward to {host}:{port} already exists with traffic that may overlap. Reuse \"{}\", edit its vehicle selection when available, or choose a different destination port.",
                    other.name
                ),
            ));
        }
    }
    if !shared.is_empty() {
        let mut ids = std::collections::BTreeMap::<u8, &Vehicle>::new();
        for vehicle in engine.state.vehicles.values().filter(|vehicle| {
            carries(desired, vehicle)
                || shared.iter().any(|link| {
                    carries(
                        scope(link.from_link.as_deref(), &link.vehicle_filter),
                        vehicle,
                    )
                })
        }) {
            if let Some(other) = ids.insert(vehicle.system_id, vehicle) {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    format!(
                        "{} and {} both use MAVLink system ID {} at {host}:{port}. Give them different system IDs or use different destination ports.",
                        other.name, vehicle.name, vehicle.system_id
                    ),
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn check(
    engine: &Engine,
    kind: &LinkKind,
    source: Option<&str>,
    vehicles: &[String],
    except: Option<&str>,
) -> Result<(), ApiError> {
    validate(engine, kind, scope(source, vehicles), except)
}
pub(crate) fn check_new_simulation(engine: &Engine, kind: &LinkKind) -> Result<(), ApiError> {
    validate(engine, kind, Scope::NewSimulation, None)
}

pub(crate) fn check_update(engine: &Engine, before: &Link, after: &Link) -> Result<(), ApiError> {
    if after.kind != before.kind || after.vehicle_filter != before.vehicle_filter {
        check(
            engine,
            &after.kind,
            after.from_link.as_deref(),
            &after.vehicle_filter,
            Some(&after.id),
        )?;
    }
    Ok(())
}

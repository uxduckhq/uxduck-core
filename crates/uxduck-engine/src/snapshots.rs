//! Simulation snapshots (S34, ADR-0011 M14): saved from a simulated
//! vehicle, started again as a new simulation at the same place. Starting
//! one adds a simulator link that names it; the take-off in `launch` writes
//! back its parameters and mission, flies to its height and restores its
//! mode.

use serde_json::{Value, json};
use uxduck_contracts::dto::link::{IdParams, LinkAddParams};
use uxduck_contracts::dto::sim::{
    SimSnapshot as SnapshotDto, SimSnapshotParams, SnapshotListResult, SnapshotRenameParams,
    SnapshotStartParams,
};
use uxduck_contracts::dto::state::{ChangeOp, EntityChange, EntityType};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::link::LinkKind;
use uxduck_core::domain::notification::{NotificationKind, Severity, Subject};
use uxduck_core::domain::snapshot::{Snapshot, SnapshotParameter};
use uxduck_core::domain::telemetry::AltitudeFrame;
use uxduck_core::events::Change;

use crate::commands::Raise;
use crate::rpc::handlers::to_value;
use crate::sim::{MAX_IN_AIR_M, MIN_IN_AIR_M};
use crate::{ConnectionId, Engine, upsert};

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::InvalidValue, message)
}

fn not_found(id: &str) -> ApiError {
    ApiError::new(ErrorKind::NotFound, format!("no snapshot {id}"))
}

fn checked_name(name: &str) -> Result<String, ApiError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(invalid("a snapshot's name is 1 to 100 characters"));
    }
    Ok(name.to_owned())
}

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    if let ClientRequest::SnapshotList(_) = request {
        if !granted.contains(&Capability::Read) {
            return Err(ApiError::new(
                ErrorKind::CapabilityDenied,
                Capability::Read.denied_message(),
            ));
        }
        let mut snapshots: Vec<&Snapshot> = engine.state.snapshots.values().collect();
        snapshots.sort_by_key(|s| std::cmp::Reverse(s.created_at_us));
        return to_value(&SnapshotListResult {
            snapshots: snapshots.into_iter().map(Into::into).collect(),
        });
    }
    if !granted.contains(&Capability::Config) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            Capability::Config.denied_message(),
        ));
    }
    match request {
        ClientRequest::SimSnapshot(params) => {
            let snapshot = save(engine, connection, &params)?;
            to_value(&SnapshotDto::from(&snapshot))
        }
        ClientRequest::SnapshotStart(params) => start(engine, connection, params),
        ClientRequest::SnapshotRename(SnapshotRenameParams { id, name }) => {
            let name = checked_name(&name)?;
            let mut snapshot = engine
                .state
                .snapshots
                .get(&id)
                .cloned()
                .ok_or_else(|| not_found(&id))?;
            if snapshot.name != name {
                snapshot.name = name;
                snapshot.version += 1;
                engine.commit(
                    connection,
                    Change::SnapshotUpserted {
                        snapshot: snapshot.clone(),
                    },
                );
            }
            to_value(&SnapshotDto::from(&snapshot))
        }
        ClientRequest::SnapshotDelete(IdParams { id }) => {
            if !engine.state.snapshots.contains_key(&id) {
                return Err(not_found(&id));
            }
            engine.commit(connection, Change::SnapshotRemoved { id });
            Ok(json!({}))
        }
        _ => Err(ApiError::new(ErrorKind::Internal, "not a snapshot method")),
    }
}

/// A vehicle's parameters as they are now.
fn saved_parameters(engine: &Engine, vehicle_id: &str) -> Vec<SnapshotParameter> {
    engine
        .state
        .parameters
        .values()
        .filter(|p| p.vehicle_id == vehicle_id)
        .map(|p| SnapshotParameter {
            name: p.name.clone(),
            value: p.value,
            kind: p.kind,
        })
        .collect()
}

/// Records a simulated vehicle as it is now.
fn save(
    engine: &mut Engine,
    connection: ConnectionId,
    params: &SimSnapshotParams,
) -> Result<Snapshot, ApiError> {
    let vehicle = engine.vehicle(&params.vehicle_id).cloned().ok_or_else(|| {
        ApiError::new(
            ErrorKind::NotFound,
            format!("no vehicle {}", params.vehicle_id),
        )
    })?;
    let Some(LinkKind::Simulator {
        vehicle: kind,
        autopilot,
        frame,
        ..
    }) = engine
        .state
        .links
        .get(&vehicle.link_id)
        .map(|l| l.kind.clone())
    else {
        return Err(invalid(format!(
            "{} is not a simulated vehicle. Only those have snapshots",
            vehicle.name
        )));
    };
    let telemetry = engine.telemetry(&vehicle.id).cloned().unwrap_or_default();
    let fused = telemetry
        .positions
        .iter()
        .find(|p| p.source == "fused")
        .ok_or_else(|| invalid(format!("{} has no position yet", vehicle.name)))?;
    let altitude = |frame: AltitudeFrame| {
        fused
            .altitudes
            .iter()
            .find(|a| a.frame == frame)
            .map(|a| a.meters)
    };
    let height_m = altitude(AltitudeFrame::AboveHome).unwrap_or(0.0);
    // Simulated ground is flat: ground level is home's.
    let ground_altitude_m = altitude(AltitudeFrame::Amsl).map_or(0.0, |amsl| amsl - height_m);
    let item = |key: &str| {
        telemetry
            .items
            .iter()
            .find(|i| i.key == key)
            .and_then(|i| i.value)
    };
    let name = match params.name.as_deref() {
        Some(name) => checked_name(name)?,
        None => format!("{} at {:.0} m", vehicle.name, height_m.max(0.0)),
    };
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a mission item's number"
    )]
    let current_item = item("mission.current").map(|seq| seq.clamp(0.0, 65_535.0) as u16);
    let snapshot = Snapshot {
        id: format!("sn{}", &engine.random.token()[..16]),
        name,
        vehicle: kind,
        autopilot,
        frame,
        firmware: vehicle.firmware.clone(),
        latitude_deg: fused.latitude_deg,
        longitude_deg: fused.longitude_deg,
        ground_altitude_m,
        height_m,
        heading_deg: fused.heading_deg.unwrap_or(0.0).rem_euclid(360.0),
        ground_speed_mps: fused.ground_speed_mps.unwrap_or(0.0),
        mode: vehicle.mode.clone(),
        armed: vehicle.armed,
        parameters: saved_parameters(engine, &vehicle.id),
        mission: engine.mission(&vehicle.id).items,
        current_item,
        operator_id: engine.actor(connection).operator_id,
        created_at_us: engine.clock.now_us(),
        version: 1,
    };
    engine.commit(
        connection,
        Change::SnapshotUpserted {
            snapshot: snapshot.clone(),
        },
    );
    announce_saved(engine, &vehicle.id, &snapshot.name);
    engine.record(
        &vehicle.id,
        uxduck_core::domain::history::HistoryEvent::SnapshotSaved {
            snapshot_id: snapshot.id.clone(),
            name: snapshot.name.clone(),
        },
    );
    Ok(snapshot)
}

/// Says a snapshot was saved, wherever it was saved from: a screen, the
/// agent or the CLI.
fn announce_saved(engine: &mut Engine, vehicle_id: &str, name: &str) {
    engine.raise(Raise {
        severity: Severity::Info,
        kind: NotificationKind::Transient,
        subject: Subject::Vehicle {
            id: vehicle_id.to_owned(),
        },
        title: format!("Saved snapshot “{name}”"),
        body: Some("Start a new simulation from it in “Run a simulation”.".to_owned()),
        actions: Vec::new(),
        condition: None,
        report: None,
    });
}

/// Adds a simulator link at the snapshot's place that names it.
fn start(
    engine: &mut Engine,
    connection: ConnectionId,
    params: SnapshotStartParams,
) -> Result<Value, ApiError> {
    let snapshot = engine
        .state
        .snapshots
        .get(&params.id)
        .cloned()
        .ok_or_else(|| not_found(&params.id))?;
    let in_air_m = (snapshot.flying()
        && !params.on_ground
        && snapshot.vehicle != uxduck_core::domain::link::SimVehicle::Airship)
        .then(|| snapshot.height_m.clamp(MIN_IN_AIR_M, MAX_IN_AIR_M));
    let add = LinkAddParams {
        name: Some(snapshot.name.clone()),
        kind: uxduck_contracts::dto::link::LinkKind::Simulator {
            vehicle: snapshot.vehicle.into(),
            autopilot: snapshot.autopilot.into(),
            latitude_deg: snapshot.latitude_deg,
            longitude_deg: snapshot.longitude_deg,
            altitude_m: snapshot.ground_altitude_m,
            heading_deg: snapshot.heading_deg,
            frame: snapshot.frame.clone(),
            speedup: params.speedup,
            in_air_m,
            snapshot_id: Some(snapshot.id.clone()),
            binary: params.binary.map(|b| b.trim().to_owned()),
            wipe: false,
            // A snapshot brings its own mission.
            with_mission: false,
            traffic: false,
            instance: None,
        },
        policy: None,
        topology: None,
        vehicle_filter: Vec::new(),
        from_link: None,
        keep_alive: false,
        carries: uxduck_contracts::dto::link::LinkCarries::Mavlink,
    };
    let link = crate::sim::start_link(engine, connection, add, params.forward)?;
    to_value(&link)
}

/// Parameters a snapshot does not write back: the simulator's own, the
/// counters, identities and per-boot calibrations, which belong to the new
/// vehicle. Its system id is `--sysid`'s, one per instance: writing the
/// snapshot's back would give it the first vehicle's system id
/// (`SYSID_THISMAV` before ArduPilot 4.7, `MAV_SYSID` since, with
/// `MAV_GCS_SYSID` for `SYSID_MYGCS`).
pub(crate) fn restorable(name: &str) -> bool {
    const KEPT: [&str; 6] = [
        "SIM_",
        "STAT_",
        "SYSID_",
        "MAV_SYSID",
        "MAV_GCS_SYSID",
        "FORMAT_VERSION",
    ];
    const PER_BOOT: [&str; 5] = ["DEV_ID", "DEVID", "GND_PRESS", "GND_ABS_PRESS", "GND_TEMP"];
    let kept = KEPT.iter().any(|p| name.starts_with(p))
        || PER_BOOT.iter().any(|p| name.contains(p))
        || (name.starts_with("INS_") && name.ends_with("_ID"));
    !kept
}

/// The wire form of a snapshot change.
pub(crate) fn entity_changes(change: &Change) -> Vec<EntityChange> {
    match change {
        Change::SnapshotUpserted { snapshot } => vec![EntityChange {
            snapshot: Some(snapshot.into()),
            ..upsert(EntityType::Snapshot, &snapshot.id)
        }],
        Change::SnapshotRemoved { id } => vec![EntityChange {
            op: ChangeOp::Remove,
            ..upsert(EntityType::Snapshot, id)
        }],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_vehicles_own_parameters_are_written_back() {
        assert!(restorable("WPNAV_SPEED"));
        assert!(restorable("TKOFF_ALT"));
        for kept in [
            "SIM_SPEEDUP",
            "STAT_RUNTIME",
            "SYSID_THISMAV",
            // ArduPilot 4.7 renamed SYSID_THISMAV and SYSID_MYGCS.
            "MAV_SYSID",
            "MAV_GCS_SYSID",
            "MAV_GCS_SYSID_HI",
            "FORMAT_VERSION",
            "COMPASS_DEV_ID",
            "BARO1_DEVID",
            "BARO1_GND_PRESS",
            "GND_ABS_PRESS",
            "INS_ACC_ID",
        ] {
            assert!(!restorable(kept), "{kept}");
        }
    }
}

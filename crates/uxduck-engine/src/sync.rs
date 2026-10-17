//! Cloud sync (S16, ADR-0001 D9, ADR-0002). The composition root's sync
//! task reads the journal after [`SyncState::exported_through`], keeps what
//! [`exportable`] picks, sends it to the account and reports back through
//! [`Engine::sync_exported`]. It fetches other computers' changes and hands
//! them to [`Engine::sync_pulled`].
//!
//! The account keeps the latest copy of each entity, and every computer
//! takes a copy by the same rule, [`wins`], so they end up alike whatever
//! order changes arrive in. Changes taken from the account are journaled as
//! `sync:<computer>` and never sent back. Vehicles go to the library; a
//! computer uses one once it is activated there, or all of them with
//! `mirror-all`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uxduck_contracts::dto::sync::{
    LibraryVehicle as LibraryVehicleDto, SyncActivation, SyncStatus,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::sync::{Activation, LibraryVehicle, SyncState};
use uxduck_core::domain::vehicle::{Presence, Vehicle};
use uxduck_core::events::{Actor, Change, Event};

use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// The actor prefix of changes taken from the account.
pub const FROM_ACCOUNT: &str = "sync:";

/// The version of a removal: it outranks every edit. The largest integer
/// JavaScript and Postgres both hold exactly.
pub const REMOVED: u64 = 9_007_199_254_740_991;

/// Where the engine tells the root there is something to send.
pub trait SyncPort: Send {
    fn wake(&mut self);
}

/// One change for the account.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncChange {
    /// `thread`, `item`, `folder`, `place`, `snapshot`, `operator`,
    /// `vehicle`, `history` or `plugin`.
    pub entity: String,
    pub id: String,
    /// Its version: [`REMOVED`] for a removal, the time of the change for a
    /// vehicle (its local version counts heartbeats, not edits).
    pub version: u64,
    /// SHA-256 of `change`, which settles two changes at one version.
    pub digest: String,
    pub ts_us: u64,
    pub change: Change,
}

/// A change another computer sent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pulled {
    /// That computer's name.
    pub device: String,
    #[serde(flatten)]
    pub change: SyncChange,
}

/// The hex SHA-256 of a change as JSON.
#[must_use]
pub fn digest(change: &Change) -> String {
    let bytes = serde_json::to_vec(change).unwrap_or_default();
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// Whether a change at `incoming` replaces the copy at `current`: a higher
/// version, or at the same version the higher digest. The account and every
/// computer decide alike.
#[must_use]
pub fn wins(incoming: (u64, &str), current: Option<(u64, &str)>) -> bool {
    current.is_none_or(|current| incoming > current)
}

/// What a vehicle is in the account: its profile, not where it was last
/// heard.
fn profile(vehicle: &Vehicle) -> Vehicle {
    Vehicle {
        aliases: Vec::new(),
        link_id: String::new(),
        peer: String::new(),
        system_id: 0,
        component_ids: Vec::new(),
        presence: Presence::Offline,
        last_heartbeat_us: 0,
        armed: false,
        mode: None,
        dismissed: false,
        version: 0,
        ..vehicle.clone()
    }
}

/// The entity a change is about, its id, and its own version (none for a
/// removal or a vehicle).
fn identify(change: &Change) -> Option<(&'static str, &str, Option<u64>)> {
    Some(match change {
        Change::ThreadUpserted { thread } => ("thread", &thread.id, Some(thread.version)),
        Change::ItemUpserted { item } => ("item", &item.id, Some(item.version)),
        Change::FolderUpserted { folder } => ("folder", &folder.id, Some(folder.version)),
        Change::PlaceUpserted { place } => ("place", &place.id, Some(place.version)),
        Change::PlaceRemoved { id } => ("place", id, None),
        Change::SnapshotUpserted { snapshot } => ("snapshot", &snapshot.id, Some(snapshot.version)),
        Change::SnapshotRemoved { id } => ("snapshot", id, None),
        Change::OperatorUpserted { operator } => ("operator", &operator.id, Some(operator.version)),
        Change::OperatorRemoved { id } => ("operator", id, None),
        Change::VehicleUpserted { vehicle } => ("vehicle", &vehicle.id, None),
        Change::HistoryUpserted { entry } => ("history", &entry.id, Some(entry.version)),
        // Its time orders it: a plugin removed and installed again wins.
        Change::PluginPublished { plugin } => ("plugin", &plugin.id, Some(plugin.at_us)),
        // A person's definition set from a URL: each computer fetches it (S74).
        Change::OwnSetUpserted { set } if set.url.is_some() => {
            ("own-set", &set.id, Some(set.updated_at_us()))
        }
        _ => return None,
    })
}

/// What the exporter does with an event.
#[derive(Clone, Debug, PartialEq)]
pub enum Outgoing {
    /// The account's to have.
    Send(Box<SyncChange>),
    /// Drop anything waiting to be sent about this entity: a vehicle just
    /// marked local-only, dismissed or no longer saved.
    Withdraw {
        entity: String,
        id: String,
    },
    Ignore,
}

/// Sorts an event for the exporter: the user's chats, folders, places,
/// simulation snapshots, operators, saved vehicles and their history go,
/// unless they came from the account. A vehicle marked local-only stays
/// here, with its history.
#[must_use]
pub fn outgoing(event: &Event) -> Outgoing {
    if event.actor.client_id.starts_with(FROM_ACCOUNT) {
        return Outgoing::Ignore;
    }
    let change = match &event.change {
        Change::VehicleUpserted { vehicle } => {
            if !vehicle.saved || vehicle.local_only || vehicle.dismissed {
                return Outgoing::Withdraw {
                    entity: "vehicle".into(),
                    id: vehicle.id.clone(),
                };
            }
            Change::VehicleUpserted {
                vehicle: profile(vehicle),
            }
        }
        // A vehicle kept here keeps its history here (ADR-0022 H1).
        Change::HistoryUpserted { entry } if entry.local => return Outgoing::Ignore,
        change => change.clone(),
    };
    let Some((entity, id, version)) = identify(&change) else {
        return Outgoing::Ignore;
    };
    let version = match (entity, version) {
        ("vehicle", _) => event.ts_us,
        (_, Some(version)) => version,
        (_, None) => REMOVED,
    };
    Outgoing::Send(Box::new(SyncChange {
        entity: entity.to_owned(),
        id: id.to_owned(),
        version,
        digest: digest(&change),
        ts_us: event.ts_us,
        change,
    }))
}

/// The change to send for an event, if any.
#[must_use]
pub fn exportable(event: &Event) -> Option<SyncChange> {
    match outgoing(event) {
        Outgoing::Send(change) => Some(*change),
        _ => None,
    }
}

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    let needed = if matches!(request, ClientRequest::SyncGet(_)) {
        Capability::Read
    } else {
        Capability::Config
    };
    if !granted.contains(&needed) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "only an operator client may change sync",
        ));
    }
    let unavailable = || ApiError::new(ErrorKind::InvalidValue, "sync needs UXDuck Desktop");
    match request {
        ClientRequest::SyncGet(_) => {}
        ClientRequest::SyncSet(params) => {
            if engine.sync.is_none() {
                return Err(unavailable());
            }
            let mut sync = engine.state.sync.clone();
            if let Some(enabled) = params.enabled {
                sync.enabled = enabled;
            }
            if sync.enabled && sync.device_id.is_empty() {
                sync.device_id = format!("dv{}", &engine.random.token()[..16]);
            }
            let mirror_all = params.activation == Some(SyncActivation::MirrorAll);
            if let Some(activation) = params.activation {
                sync.activation = match activation {
                    SyncActivation::Pick => Activation::Pick,
                    SyncActivation::MirrorAll => Activation::MirrorAll,
                };
            }
            if sync != engine.state.sync {
                sync.version += 1;
                engine.commit(connection, Change::SyncChanged { sync });
            }
            if mirror_all {
                let waiting: Vec<LibraryVehicle> = engine
                    .state
                    .library
                    .values()
                    .filter(|v| !engine.used_here(&v.id))
                    .cloned()
                    .collect();
                for entry in waiting {
                    engine.activate(connection, &entry);
                }
            }
            engine.wake_sync();
        }
        ClientRequest::SyncActivate(params) => {
            let entry = engine
                .state
                .library
                .get(&params.id)
                .cloned()
                .ok_or_else(|| {
                    ApiError::new(ErrorKind::NotFound, "no such vehicle in the account")
                })?;
            engine.activate(connection, &entry);
        }
        _ => return Err(ApiError::new(ErrorKind::Internal, "not a sync method")),
    }
    to_value(&engine.sync_status())
}

impl Engine {
    /// Connects the root's sync task (S16). Without it, sync is unavailable.
    pub fn set_sync(&mut self, port: Box<dyn SyncPort>) {
        self.sync = Some(port);
    }

    /// Whether sync is on, where it has got, and this computer's id there.
    #[must_use]
    pub fn sync_state(&self) -> &SyncState {
        &self.state.sync
    }

    /// Why sync is not working, or `None` after a round that worked. Not
    /// journaled: it describes now, not what happened, and a round that
    /// found nothing to do leaves no event.
    pub fn sync_problem(&mut self, problem: Option<String>) {
        if problem.is_none() {
            self.sync_checked_us = Some(self.clock.now_us());
        }
        self.sync_problem = problem;
    }

    /// The account has this computer's changes up to `through`: `sent` of
    /// them were its to have. Journaled when something was sent, so what
    /// left is on the record and the cursor survives a restart.
    pub fn sync_exported(&mut self, through: u64, sent: u64) {
        if sent == 0 || through <= self.state.sync.exported_through {
            return;
        }
        let mut sync = self.state.sync.clone();
        sync.exported_through = through;
        sync.exported += sent;
        sync.last_synced_at_us = Some(self.clock.now_us());
        sync.version += 1;
        self.sync_problem = None;
        self.commit_as(sync_actor("export"), Change::SyncChanged { sync });
    }

    /// Other computers' changes, fetched up to the account's `cursor`.
    /// Each is taken if it wins against the copy here; then the cursor
    /// moves.
    pub fn sync_pulled(&mut self, changes: Vec<Pulled>, cursor: u64) {
        let mut taken = 0;
        for pulled in changes {
            if self.take(&pulled) {
                taken += 1;
            }
        }
        if cursor <= self.state.sync.pulled_through && taken == 0 {
            return;
        }
        let mut sync = self.state.sync.clone();
        sync.pulled_through = sync.pulled_through.max(cursor);
        sync.pulled += taken;
        sync.last_synced_at_us = Some(self.clock.now_us());
        sync.version += 1;
        self.sync_problem = None;
        self.commit_as(sync_actor("pull"), Change::SyncChanged { sync });
    }

    fn take(&mut self, pulled: &Pulled) -> bool {
        let change = &pulled.change;
        // What the change is must match what it says it is.
        let Some((entity, id, own_version)) = identify(&change.change) else {
            return false;
        };
        if entity != change.entity
            || id != change.id
            || digest(&change.change) != change.digest
            || own_version.is_some_and(|version| version != change.version)
        {
            return false;
        }
        let actor = Actor {
            client_id: format!("{FROM_ACCOUNT}{}", pulled.device),
            operator_id: None,
        };
        if let Change::VehicleUpserted { vehicle } = &change.change {
            return self.take_library(actor, &pulled.device, vehicle, change.version);
        }
        let current = self.current(entity, id);
        if change.version == REMOVED && current.is_none() {
            return false;
        }
        let current = current.as_ref().map(|(v, d)| (*v, d.as_str()));
        if !wins((change.version, &change.digest), current) {
            return false;
        }
        self.commit_as(actor.clone(), change.change.clone());
        if let Change::PluginPublished { plugin } = &change.change {
            self.published_taken(&actor, plugin);
        }
        // A set from another computer: fetched here from its URL (S74).
        if let Change::OwnSetUpserted { set } = &change.change {
            self.fetch_own_if_missing(set);
        }
        true
    }

    /// The version and digest of the copy here.
    fn current(&self, entity: &str, id: &str) -> Option<(u64, String)> {
        let state = &self.state;
        let (version, change) = match entity {
            "thread" => state
                .threads
                .get(id)
                .map(|t| (t.version, Change::ThreadUpserted { thread: t.clone() }))?,
            "item" => state
                .items
                .get(id)
                .map(|i| (i.version, Change::ItemUpserted { item: i.clone() }))?,
            "folder" => state
                .folders
                .get(id)
                .map(|f| (f.version, Change::FolderUpserted { folder: f.clone() }))?,
            "place" => state
                .places
                .get(id)
                .map(|p| (p.version, Change::PlaceUpserted { place: p.clone() }))?,
            "snapshot" => state.snapshots.get(id).map(|s| {
                (
                    s.version,
                    Change::SnapshotUpserted {
                        snapshot: s.clone(),
                    },
                )
            })?,
            "operator" => state.operators.get(id).map(|o| {
                (
                    o.version,
                    Change::OperatorUpserted {
                        operator: o.clone(),
                    },
                )
            })?,
            "history" => state
                .history
                .get(id)
                .map(|e| (e.version, Change::HistoryUpserted { entry: e.clone() }))?,
            "plugin" => state
                .published
                .get(id)
                .map(|p| (p.at_us, Change::PluginPublished { plugin: p.clone() }))?,
            "own-set" => state
                .own_sets
                .get(id)
                .filter(|s| s.url.is_some())
                .map(|s| (s.updated_at_us(), Change::OwnSetUpserted { set: s.clone() }))?,
            _ => return None,
        };
        Some((version, digest(&change)))
    }

    /// A vehicle from the account: into the library if newer, and onto the
    /// vehicle here if it is used here (or `mirror-all` says to use it).
    fn take_library(&mut self, actor: Actor, device: &str, vehicle: &Vehicle, at: u64) -> bool {
        if self
            .state
            .library
            .get(&vehicle.id)
            .is_some_and(|entry| entry.updated_at_us >= at)
        {
            return false;
        }
        let entry = LibraryVehicle {
            id: vehicle.id.clone(),
            name: vehicle.name.clone(),
            autopilot: vehicle.autopilot.clone(),
            vehicle_type: vehicle.vehicle_type.clone(),
            uid: vehicle.uid.clone(),
            firmware: vehicle.firmware.clone(),
            size_cm: vehicle.size_cm,
            device: device.to_owned(),
            updated_at_us: at,
        };
        self.commit_as(
            actor.clone(),
            Change::LibraryUpserted {
                vehicle: entry.clone(),
            },
        );
        match self.state.vehicles.get(&entry.id) {
            // The person's own fields follow: its name and its size.
            Some(here)
                if here.saved && (here.name != entry.name || here.size_cm != entry.size_cm) =>
            {
                let mut here = here.clone();
                here.name.clone_from(&entry.name);
                here.size_cm = entry.size_cm;
                here.version += 1;
                self.commit_as(actor, Change::VehicleUpserted { vehicle: here });
            }
            Some(here) if here.saved => {}
            _ if self.state.sync.activation == Activation::MirrorAll => {
                self.activate_as(actor, &entry);
            }
            _ => {}
        }
        true
    }

    fn used_here(&self, id: &str) -> bool {
        self.state.vehicles.get(id).is_some_and(|v| v.saved)
    }

    /// Uses a library vehicle here: saved, offline until it is heard.
    fn activate(&mut self, connection: ConnectionId, entry: &LibraryVehicle) {
        let actor = Actor {
            client_id: format!("{FROM_ACCOUNT}library"),
            operator_id: self.actor(connection).operator_id,
        };
        self.activate_as(actor, entry);
    }

    /// As [`Engine::activate`], journaled as coming from the account, so it
    /// is not sent back to it.
    fn activate_as(&mut self, actor: Actor, entry: &LibraryVehicle) {
        let vehicle = match self.state.vehicles.get(&entry.id) {
            Some(here) if here.saved => return,
            Some(here) => Vehicle {
                name: entry.name.clone(),
                size_cm: entry.size_cm,
                saved: true,
                dismissed: false,
                version: here.version + 1,
                ..here.clone()
            },
            None => Vehicle {
                id: entry.id.clone(),
                name: entry.name.clone(),
                aliases: Vec::new(),
                link_id: String::new(),
                peer: String::new(),
                system_id: 0,
                component_ids: Vec::new(),
                autopilot: entry.autopilot.clone(),
                vehicle_type: entry.vehicle_type.clone(),
                uid: entry.uid.clone(),
                firmware: entry.firmware.clone(),
                firmware_commit: None,
                presence: Presence::Offline,
                last_heartbeat_us: 0,
                armed: false,
                mode: None,
                saved: true,
                dismissed: false,
                local_only: false,
                labels: Vec::new(),
                bookmarks: Vec::new(),
                size_cm: entry.size_cm,
                version: 1,
            },
        };
        self.commit_as(actor, Change::VehicleUpserted { vehicle });
    }

    /// Tells the root's sync task to look for something to send.
    pub(crate) fn wake_sync(&mut self) {
        if self.state.sync.enabled
            && let Some(port) = self.sync.as_mut()
        {
            port.wake();
        }
    }

    /// Whether an event just committed is one to send soon.
    pub(crate) fn wake_sync_for(&mut self, event: &Event) {
        if self.state.sync.enabled && self.sync.is_some() && exportable(event).is_some() {
            self.wake_sync();
        }
    }

    #[must_use]
    pub fn sync_status(&self) -> SyncStatus {
        let sync = &self.state.sync;
        let available = self.sync.is_some();
        SyncStatus {
            available,
            enabled: sync.enabled && available,
            activation: match sync.activation {
                Activation::Pick => SyncActivation::Pick,
                Activation::MirrorAll => SyncActivation::MirrorAll,
            },
            exported: sync.exported,
            pulled: sync.pulled,
            last_synced_at_us: sync.last_synced_at_us.max(self.sync_checked_us),
            problem: if available {
                self.sync_problem.clone()
            } else {
                Some("Sync needs UXDuck installed on a computer.".into())
            },
            library: self
                .state
                .library
                .values()
                .map(|v| LibraryVehicleDto {
                    id: v.id.clone(),
                    name: v.name.clone(),
                    autopilot: v.autopilot.clone(),
                    vehicle_type: v.vehicle_type.clone(),
                    uid: v.uid.clone(),
                    firmware: v.firmware.clone(),
                    device: v.device.clone(),
                    updated_at_us: v.updated_at_us,
                    active: self.used_here(&v.id),
                })
                .collect(),
        }
    }
}

fn sync_actor(what: &str) -> Actor {
    Actor {
        client_id: format!("{FROM_ACCOUNT}{what}"),
        operator_id: None,
    }
}

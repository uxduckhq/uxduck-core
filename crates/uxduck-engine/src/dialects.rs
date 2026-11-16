//! MAVLink dialects people add for their forks (ADR-0044, S252): read from
//! XML into the messages that are new or differ from the built-in dialect,
//! journaled, used by the vehicles whose commit they match or that chose
//! them (F13), and published to [`uxduck_mavlink::custom`]
//! for the hosts' inspectors, `mavlink/history`, plugins and the
//! reassembler.

use std::sync::Arc;

use serde_json::{Value, json};
use uxduck_contracts::dto::dialect::{
    self as dto, DialectAddParams, DialectBindParams, DialectsListResult, DialectsUseParams,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::dialect::{Dialect, DialectChoice, MessageChange};
use uxduck_core::events::{Actor, Change};
use uxduck_mavlink::custom;

use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// The most dialect XML read at once, in bytes: the files of one
/// `mavlink/dialects/add`, or those a firmware profile names. The files
/// come from a fork's repository, not from UXDuck, and what is kept of them
/// is journaled whole, sent whole to each window that lists dialects, and
/// built into the messages of the vehicles it describes, all on the
/// engine's own thread. 8 MB of the smallest messages keeps 107,592 of them
/// in a record of 10.9 MB, read in 0.2 s and built in 45 ms (measured
/// 2026-11-15, release build, Linux, 4 processors). ArduPilot's whole
/// set is 0.8 MB and keeps 11 messages, PX4's 0.8 MB and 30. More is refused
/// with its measured size.
///
/// It is the one bound on a dialect. How many dialects there are, how many
/// commits one matches and how many messages, enums, entries and commands
/// one keeps have no fixed number (maintainer, 2026-11-15).
pub(crate) const MAX_XML_BYTES: usize = 8 << 20;

/// Why this much dialect XML is not read, with its measured size, or None
/// when it is.
pub(crate) fn too_much_xml(bytes: usize) -> Option<String> {
    (bytes > MAX_XML_BYTES).then(|| {
        // Rounded up, so a size just over never reads as 8.0 MB.
        let tenths = bytes.saturating_mul(10).div_ceil(1 << 20);
        format!(
            "the files are {}.{} MB together, more than the {} MB of dialect XML UXDuck reads \
             at once. ArduPilot's whole set is under 1 MB",
            tenths / 10,
            tenths % 10,
            MAX_XML_BYTES >> 20
        )
    })
}

/// A vehicle's dialects' ids and versions, in the order they apply.
type MadeFrom = Vec<(String, u64)>;

/// What each vehicle's messages were last published from, and the messages.
/// A dialect has no fixed number of messages, so a large one is built once
/// for the vehicles it describes and not once for each, nor again each time
/// a vehicle changes. A dialect's version moves whenever it does.
#[derive(Debug, Default)]
pub(crate) struct Published(
    std::sync::Mutex<std::collections::HashMap<String, (MadeFrom, Arc<custom::Messages>)>>,
);

fn actor() -> Actor {
    Actor {
        client_id: "dialects".into(),
        operator_id: None,
        identity: None,
    }
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::InvalidValue, message)
}

impl Engine {
    /// The dialects a vehicle uses, by its choice and its firmware's
    /// commit, in the order they apply: oldest first, the newest winning.
    pub(crate) fn vehicle_dialects(&self, vehicle_id: &str) -> Vec<&Dialect> {
        let Some(vehicle) = self.state.vehicles.get(vehicle_id) else {
            return Vec::new();
        };
        let mut found: Vec<&Dialect> = self
            .state
            .dialects
            .values()
            .filter(|d| d.describes(&vehicle.dialects, vehicle.firmware_commit.as_deref()))
            .collect();
        found.sort_by(|a, b| a.added_at_us.cmp(&b.added_at_us).then(a.id.cmp(&b.id)));
        found
    }

    /// The commands a plan's dialects declare without a place
    /// (`hasLocation="false"`): those for every vehicle, then the plan's,
    /// the last to define a number winning, as the plan's editor reads them.
    pub(crate) fn placeless_commands(&self, plan_id: &str) -> Vec<u16> {
        let Some(plan) = self.state.plans.get(plan_id) else {
            return Vec::new();
        };
        let mut every: Vec<&Dialect> = self
            .state
            .dialects
            .values()
            .filter(|d| d.every_vehicle)
            .collect();
        every.sort_by(|a, b| a.added_at_us.cmp(&b.added_at_us).then(a.id.cmp(&b.id)));
        let own = plan
            .body
            .dialects
            .iter()
            .filter_map(|id| self.state.dialects.get(id).filter(|d| !d.every_vehicle));
        let mut placeless = std::collections::BTreeMap::new();
        for dialect in every.into_iter().chain(own) {
            for command in &dialect.commands {
                placeless.insert(command.value, command.no_location);
            }
        }
        placeless
            .into_iter()
            .filter_map(|(value, no_location)| no_location.then_some(value))
            .collect()
    }

    /// The vehicles using a dialect now, by name.
    fn used_by(&self, dialect: &Dialect) -> Vec<String> {
        let mut using: Vec<&uxduck_core::domain::vehicle::Vehicle> = self
            .state
            .vehicles
            .values()
            .filter(|v| !v.replay && dialect.describes(&v.dialects, v.firmware_commit.as_deref()))
            .collect();
        using.sort_by(|a, b| a.name.cmp(&b.name));
        using.into_iter().map(|v| v.id.clone()).collect()
    }

    /// A dialect for the wire, with the vehicles using it and the standard
    /// messages UXDuck's own readers lose by it (F5).
    pub(crate) fn dialect_summary(&self, dialect: &Dialect) -> dto::Dialect {
        dto::Dialect {
            used_by: self.used_by(dialect),
            ..summary(dialect)
        }
    }

    /// Tells the decoders which messages describe this vehicle. Nothing is
    /// built when its dialects are those it was last published with, and the
    /// vehicles the same dialects describe share one copy of the messages.
    fn publish_vehicle_dialects(&self, vehicle_id: &str) {
        let dialects = self.vehicle_dialects(vehicle_id);
        let made_from: MadeFrom = dialects
            .iter()
            .map(|d| (d.id.clone(), d.updated_at_us))
            .collect();
        let mut published = self
            .published_dialects
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if published
            .get(vehicle_id)
            .is_some_and(|(from, _)| *from == made_from)
        {
            return;
        }
        let messages = published
            .values()
            .find(|(from, _)| *from == made_from)
            .map_or_else(
                || Arc::new(custom::Messages::of(dialects)),
                |(_, messages)| messages.clone(),
            );
        custom::publish_shared(vehicle_id, messages.clone());
        published.insert(vehicle_id.to_owned(), (made_from, messages));
    }

    /// Every vehicle's messages and every dialect's checksums, as the
    /// state has them, at start.
    pub(crate) fn publish_dialects(&self) {
        for dialect in self.state.dialects.values() {
            custom::publish_checksums(dialect);
        }
        self.publish_vehicles_dialects();
    }

    fn publish_vehicles_dialects(&self) {
        for id in self.state.vehicles.keys() {
            self.publish_vehicle_dialects(id);
        }
    }

    /// After a change was applied: what it means for the decoders. Only the
    /// dialect that changed is published again, so adding one costs the same
    /// however many there are.
    pub(crate) fn dialects_after(&self, change: &Change) {
        match change {
            Change::DialectUpserted { dialect } => {
                custom::publish_checksums(dialect);
                self.publish_vehicles_dialects();
            }
            Change::DialectRemoved { id } => {
                custom::forget_checksums(id);
                self.publish_vehicles_dialects();
            }
            Change::VehicleUpserted { vehicle } => self.publish_vehicle_dialects(&vehicle.id),
            Change::VehicleRemoved { id } => {
                self.published_dialects
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(id);
                custom::publish(id, custom::Messages::default());
            }
            _ => {}
        }
    }

    /// Refuses to change a dialect a firmware profile brought on its own
    /// (S254): it goes with its profile.
    fn not_a_profiles(&self, dialect: &Dialect) -> Result<(), ApiError> {
        match &dialect.profile_id {
            Some(profile) => Err(invalid(format!(
                "this dialect came with the firmware profile {}. Remove the profile instead",
                self.state
                    .firmware_profiles
                    .get(profile)
                    .map_or(profile.as_str(), |p| p.label.as_str())
            ))),
            None => Ok(()),
        }
    }

    /// Which vehicles use a dialect by itself, checked: commits of hex,
    /// and no change to a standard message for every vehicle (F3).
    pub(crate) fn reach(
        dialect: &Dialect,
        commits: &[String],
        every_vehicle: bool,
    ) -> Result<Vec<String>, ApiError> {
        if every_vehicle {
            let changed: Vec<&str> = dialect
                .messages
                .iter()
                .filter(|m| m.change != MessageChange::New)
                .map(|m| m.name.as_str())
                .collect();
            if !changed.is_empty() {
                return Err(invalid(format!(
                    "{} changes {}, so it can't be for every vehicle. Give the commits of the \
                     firmware that has it, or choose it for each vehicle in its setup",
                    dialect.label,
                    changed.join(", ")
                )));
            }
            return Ok(Vec::new());
        }
        // However many commits: a fork's every build may be named.
        let mut kept: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for commit in commits {
            let c = commit.trim().to_ascii_lowercase();
            if c.is_empty() {
                continue;
            }
            if c.len() < 7 || c.len() > 40 || !c.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(invalid(format!(
                    "{c} is not a commit: a commit is 7 to 40 hex characters"
                )));
            }
            if seen.insert(c.clone()) {
                kept.push(c);
            }
        }
        Ok(kept)
    }

    /// Chooses a dialect for a vehicle beside those it uses now, unless it
    /// already does.
    fn choose_dialect(&mut self, connection: ConnectionId, vehicle_id: &str, id: &str) {
        let Some(vehicle) = self.state.vehicles.get(vehicle_id) else {
            return;
        };
        let commit = vehicle.firmware_commit.as_deref();
        if self
            .state
            .dialects
            .get(id)
            .is_none_or(|d| d.describes(&vehicle.dialects, commit))
        {
            return;
        }
        let mut ids: Vec<String> = self
            .vehicle_dialects(vehicle_id)
            .into_iter()
            .filter(|d| !d.every_vehicle)
            .map(|d| d.id.clone())
            .collect();
        ids.push(id.to_owned());
        let mut vehicle = vehicle.clone();
        vehicle.dialects = DialectChoice::Chosen { ids };
        vehicle.version += 1;
        self.commit(connection, Change::VehicleUpserted { vehicle });
    }

    /// `mavlink/dialects/use`: a vehicle's choice, checked and kept.
    fn use_dialects(
        &mut self,
        connection: ConnectionId,
        params: DialectsUseParams,
    ) -> Result<DialectsListResult, ApiError> {
        let mut vehicle = self.vehicle(&params.vehicle_id).cloned().ok_or_else(|| {
            ApiError::new(
                ErrorKind::NotFound,
                format!("no vehicle {}", params.vehicle_id),
            )
        })?;
        let choice = match DialectChoice::from(params.choice) {
            DialectChoice::Automatic => DialectChoice::Automatic,
            DialectChoice::Chosen { ids } => {
                let mut kept: Vec<String> = Vec::new();
                for id in ids {
                    if !self.state.dialects.contains_key(&id) {
                        return Err(ApiError::new(
                            ErrorKind::NotFound,
                            format!("no dialect {id}"),
                        ));
                    }
                    if !kept.contains(&id) {
                        kept.push(id);
                    }
                }
                DialectChoice::Chosen { ids: kept }
            }
        };
        if choice != vehicle.dialects {
            vehicle.dialects = choice;
            vehicle.version += 1;
            self.commit(connection, Change::VehicleUpserted { vehicle });
        }
        Ok(self.vehicle_list(&params.vehicle_id))
    }

    /// The dialects a vehicle uses, with its choice.
    fn vehicle_list(&self, vehicle_id: &str) -> DialectsListResult {
        DialectsListResult {
            dialects: self
                .vehicle_dialects(vehicle_id)
                .into_iter()
                .map(|d| self.dialect_summary(d))
                .collect(),
            choice: self
                .state
                .vehicles
                .get(vehicle_id)
                .map(|v| (&v.dialects).into()),
        }
    }

    fn add_dialect(
        &mut self,
        connection: ConnectionId,
        params: &DialectAddParams,
    ) -> Result<Dialect, ApiError> {
        let vehicle_id = params
            .vehicle_id
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty());
        if let Some(id) = vehicle_id
            && !self.state.vehicles.contains_key(id)
        {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                format!("no vehicle {id}"),
            ));
        }
        let bytes: usize = params.files.iter().map(|f| f.text.len()).sum();
        if let Some(why) = too_much_xml(bytes) {
            return Err(invalid(why));
        }
        let files: Vec<(&str, &str)> = params
            .files
            .iter()
            .map(|f| (f.name.as_str(), f.text.as_str()))
            .collect();
        let read = custom::read(&files).map_err(invalid)?;
        let label = params
            .label
            .as_deref()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map_or_else(
                || {
                    params.files.first().map_or_else(
                        || "Dialect".to_owned(),
                        |f| f.name.trim_end_matches(".xml").to_owned(),
                    )
                },
                str::to_owned,
            );
        // A name a person types is kept whole, however long (2026-11-15).
        let files: Vec<String> = params.files.iter().map(|f| f.name.clone()).collect();
        let older = self
            .state
            .dialects
            .values()
            .find(|d| add_again(d) && d.label == label && d.files == files)
            .cloned();
        let now = self.clock.now_us();
        let mut dialect = Dialect {
            id: older.as_ref().map_or_else(
                || format!("dialect-{}", &self.random.token()[..12]),
                |d| d.id.clone(),
            ),
            label,
            files,
            commits: Vec::new(),
            every_vehicle: params.every_vehicle,
            messages: read.messages,
            enums: read.enums,
            commands: read.commands,
            unchanged: read.unchanged,
            added_at_us: older.as_ref().map_or(now, |d| d.added_at_us),
            updated_at_us: older.as_ref().map_or(now, |d| now.max(d.updated_at_us + 1)),
            profile_id: None,
            places_read: true,
        };
        dialect.commits = Self::reach(&dialect, &params.commits, params.every_vehicle)?;
        self.commit_as(
            actor(),
            Change::DialectUpserted {
                dialect: dialect.clone(),
            },
        );
        if let Some(id) = vehicle_id {
            self.choose_dialect(connection, id, &dialect.id);
        }
        Ok(dialect)
    }

    fn bind_dialect(&mut self, params: &DialectBindParams) -> Result<Dialect, ApiError> {
        let mut dialect = self
            .state
            .dialects
            .get(&params.id)
            .cloned()
            .ok_or_else(|| {
                ApiError::new(ErrorKind::NotFound, format!("no dialect {}", params.id))
            })?;
        self.not_a_profiles(&dialect)?;
        dialect.commits = Self::reach(&dialect, &params.commits, params.every_vehicle)?;
        dialect.every_vehicle = params.every_vehicle;
        dialect.updated_at_us = self.clock.now_us().max(dialect.updated_at_us + 1);
        self.commit_as(
            actor(),
            Change::DialectUpserted {
                dialect: dialect.clone(),
            },
        );
        Ok(dialect)
    }
}

/// Whether a dialect an older UXDuck stored must be added again for its
/// commands without a place to be verified on readback: one a person added
/// (a firmware profile's is read again when it is fetched) with a command
/// its file may have said has no place.
fn add_again(dialect: &Dialect) -> bool {
    !dialect.places_read
        && dialect.profile_id.is_none()
        && dialect
            .commands
            .iter()
            .any(|c| !c.has_location && custom::may_be_placeless(c.value))
}

/// A dialect for the wire, with the standard messages UXDuck's own readers
/// lose by it (F5).
pub(crate) fn summary(dialect: &Dialect) -> dto::Dialect {
    dto::Dialect {
        add_again: add_again(dialect),
        unread: dialect
            .messages
            .iter()
            .filter(|m| m.change == MessageChange::Layout && uxduck_mavlink::decode::reads(m.id))
            .map(|m| m.name.clone())
            .collect(),
        ..dialect.into()
    }
}

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
        ClientRequest::MavlinkDialectsList(params) => {
            require(Capability::Read)?;
            if let Some(id) = params.vehicle_id.as_deref() {
                if engine.vehicle(id).is_none() {
                    return Err(ApiError::new(
                        ErrorKind::NotFound,
                        format!("no vehicle {id}"),
                    ));
                }
                return to_value(&engine.vehicle_list(id));
            }
            let mut all: Vec<&Dialect> = engine.state.dialects.values().collect();
            all.sort_by_key(|d| std::cmp::Reverse(d.added_at_us));
            to_value(&DialectsListResult {
                dialects: all.into_iter().map(|d| engine.dialect_summary(d)).collect(),
                choice: None,
            })
        }
        ClientRequest::MavlinkDialectsAdd(params) => {
            require(Capability::Config)?;
            let dialect = engine.add_dialect(connection, &params)?;
            to_value(&engine.dialect_summary(&dialect))
        }
        ClientRequest::MavlinkDialectsBind(params) => {
            require(Capability::Config)?;
            let dialect = engine.bind_dialect(&params)?;
            to_value(&engine.dialect_summary(&dialect))
        }
        ClientRequest::MavlinkDialectsUse(params) => {
            require(Capability::Config)?;
            to_value(&engine.use_dialects(connection, params)?)
        }
        ClientRequest::MavlinkDialectsRemove(params) => {
            require(Capability::Config)?;
            let Some(dialect) = engine.state.dialects.get(&params.id) else {
                return Err(ApiError::new(
                    ErrorKind::NotFound,
                    format!("no dialect {}", params.id),
                ));
            };
            engine.not_a_profiles(dialect)?;
            engine.commit_as(actor(), Change::DialectRemoved { id: params.id });
            Ok(json!({}))
        }
        _ => Err(ApiError::new(ErrorKind::Internal, "not a dialects method")),
    }
}

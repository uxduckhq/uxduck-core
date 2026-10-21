//! A vehicle's history (ADR-0022, S69): notes people write, kept with their
//! earlier versions and marked rather than removed when deleted, and events
//! UXDuck records by itself. Called history, never log.

use serde_json::Value;
use uxduck_contracts::dto::history::{
    AddNoteParams, EditNoteParams, HistoryEntry as HistoryEntryDto, VehicleHistoryParams,
    VehicleHistoryResult,
};
use uxduck_contracts::dto::link::IdParams;
use uxduck_contracts::dto::state::{EntityChange, EntityType};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::history::{
    Author, Deletion, EntryKind, HistoryEntry, HistoryEvent, NoteVersion, ParamSource,
};
use uxduck_core::domain::vehicle::Vehicle;
use uxduck_core::events::{Actor, Change};

use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine, upsert};

/// A note's words: enough for a repair, not a report.
const MAX_TEXT: usize = 2000;
const MAX_TAGS: usize = 10;
const DEFAULT_ENTRIES: u32 = 20;
const MAX_ENTRIES: u32 = 200;
/// Changes of one parameter this close to the last are one sitting (H3)
/// and fold into one entry, so a value that flaps and a person trying
/// values read as one line each, not a hundred.
const SITTING_US: u64 = 10 * 60 * 1_000_000;

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::InvalidValue, message)
}

/// UXDuck itself, for the events it records.
fn uxduck() -> Author {
    Author {
        client_name: "UXDuck".into(),
        ..Author::default()
    }
}

pub(crate) fn actor() -> Actor {
    Actor {
        client_id: "history".into(),
        operator_id: None,
        identity: None,
    }
}

/// Whether an entry belongs to this vehicle, under its id or one it had.
fn belongs(entry: &HistoryEntry, vehicle: &Vehicle) -> bool {
    entry.vehicle_id == vehicle.id || vehicle.aliases.contains(&entry.vehicle_id)
}

/// The entry's parameter change, if it is one.
fn param_change(entry: &HistoryEntry) -> Option<&HistoryEvent> {
    match &entry.kind {
        EntryKind::Event {
            event: event @ HistoryEvent::ParameterChanged { .. },
        } => Some(event),
        _ => None,
    }
}

/// When a parameter entry's latest change happened: its time, plus how
/// long the changes folded into it spread over.
fn last_change_us(entry: &HistoryEntry) -> u64 {
    let span = match param_change(entry) {
        Some(HistoryEvent::ParameterChanged { span_us, .. }) => span_us.unwrap_or(0),
        _ => 0,
    };
    entry.happened_at_us.saturating_add(span)
}

impl Engine {
    /// Who a connection writes as: its client, and the operator it acts for.
    pub(crate) fn author(&self, connection: ConnectionId) -> Author {
        let Some(c) = self.connections.get(&connection) else {
            return uxduck();
        };
        let operator_name = c
            .operator_id
            .as_ref()
            .and_then(|id| self.state.operators.get(id))
            .map(|o| o.name.clone());
        Author {
            client_name: c.name.clone(),
            client_kind: c.admitted.as_ref().and_then(|a| {
                serde_json::to_value(uxduck_contracts::dto::initialize::ClientKind::from(a.kind))
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
            }),
            operator_id: c.operator_id.clone(),
            operator_name,
        }
    }

    /// A vehicle's history stays on this computer while the vehicle does.
    fn kept_local(&self, vehicle_id: &str) -> bool {
        self.state
            .vehicles
            .get(vehicle_id)
            .is_none_or(|v| v.local_only || !v.saved)
    }

    /// Records an event in a vehicle's history (H3).
    pub(crate) fn record(&mut self, vehicle_id: &str, event: HistoryEvent) {
        self.record_by(vehicle_id, event, uxduck());
    }

    /// Records an event someone caused, such as a parameter they set (S74).
    pub(crate) fn record_by(&mut self, vehicle_id: &str, event: HistoryEvent, author: Author) {
        let now = self.clock.now_us();
        let entry = HistoryEntry {
            id: format!("hi{}", &self.random.token()[..16]),
            vehicle_id: vehicle_id.to_owned(),
            kind: EntryKind::Event { event },
            happened_at_us: now,
            written_at_us: now,
            author,
            edited_at_us: None,
            deleted: None,
            local: self.kept_local(vehicle_id),
            version: 1,
        };
        self.commit_as(actor(), Change::HistoryUpserted { entry });
    }

    /// Records a parameter's change of value (S74). A change within a
    /// sitting of the parameter's last one, by the same source and author
    /// for the same reason, folds into that entry: it then says the value
    /// before the first change, the value after the last, how many there
    /// were and over how long. Anyone else's change in between starts a new
    /// entry.
    pub(crate) fn record_param_change(
        &mut self,
        vehicle_id: &str,
        name: &str,
        (from, to): (Option<f64>, f64),
        source: ParamSource,
        reason: Option<String>,
        author: Option<Author>,
    ) {
        let now = self.clock.now_us();
        let author = author.unwrap_or_else(uxduck);
        let aliases = self
            .state
            .vehicles
            .get(vehicle_id)
            .map(|v| v.aliases.clone())
            .unwrap_or_default();
        let last = self
            .state
            .history
            .values()
            .filter(|e| e.vehicle_id == vehicle_id || aliases.contains(&e.vehicle_id))
            .filter(|e| e.deleted.is_none())
            .filter(|e| {
                matches!(param_change(e), Some(HistoryEvent::ParameterChanged { name: n, .. }) if n == name)
            })
            .max_by_key(|e| (last_change_us(e), e.written_at_us));
        let same_sitting = last
            .filter(|e| {
                matches!(
                    param_change(e),
                    Some(HistoryEvent::ParameterChanged { source: s, reason: r, .. })
                        if *s == source && *r == reason
                )
            })
            .filter(|e| e.author == author)
            .filter(|e| now.saturating_sub(last_change_us(e)) <= SITTING_US)
            .cloned();
        if let Some(mut entry) = same_sitting {
            if let EntryKind::Event {
                event:
                    HistoryEvent::ParameterChanged {
                        to: latest,
                        changes,
                        span_us,
                        ..
                    },
            } = &mut entry.kind
            {
                *latest = to;
                *changes = Some(changes.unwrap_or(1).saturating_add(1));
                *span_us = Some(now.saturating_sub(entry.happened_at_us));
            }
            entry.version += 1;
            self.commit_as(actor(), Change::HistoryUpserted { entry });
            return;
        }
        let event = HistoryEvent::ParameterChanged {
            name: name.to_owned(),
            from,
            to,
            source,
            reason,
            changes: None,
            span_us: None,
        };
        self.record_by(vehicle_id, event, author);
    }

    /// What a newly seen or changed vehicle adds to its history: first
    /// seen, renamed, its firmware or type changed. Called with the record
    /// before and after.
    pub(crate) fn record_vehicle_change(&mut self, before: Option<&Vehicle>, after: &Vehicle) {
        // First seen once it has its lasting identity, its UID: a vehicle
        // already known is heard under a provisional id after each restart
        // until its UID comes, and that record is not a new vehicle.
        let identified = after.uid.is_some() && before.is_none_or(|b| b.uid.is_none());
        if identified && !self.state.history.values().any(|e| belongs(e, after)) {
            self.record(
                &after.id,
                HistoryEvent::FirstSeen {
                    vehicle_type: after.vehicle_type.clone(),
                },
            );
        }
        let Some(before) = before else {
            return;
        };
        if before.name != after.name {
            self.record(
                &after.id,
                HistoryEvent::Renamed {
                    from: before.name.clone(),
                    to: after.name.clone(),
                },
            );
        }
        if let (Some(from), Some(to)) = (&before.firmware, &after.firmware)
            && (from != to
                || (before.firmware_commit != after.firmware_commit
                    && after.firmware_commit.is_some()))
        {
            self.record(
                &after.id,
                HistoryEvent::FirmwareChanged {
                    from: Some(from.clone()),
                    to: to.clone(),
                    commit: after.firmware_commit.clone(),
                },
            );
        }
        if !before.vehicle_type.is_empty() && before.vehicle_type != after.vehicle_type {
            self.record(
                &after.id,
                HistoryEvent::TypeChanged {
                    from: before.vehicle_type.clone(),
                    to: after.vehicle_type.clone(),
                },
            );
        }
    }

    fn add_note(
        &mut self,
        connection: ConnectionId,
        params: AddNoteParams,
    ) -> Result<HistoryEntry, ApiError> {
        let vehicle = self.vehicle(&params.vehicle_id).cloned().ok_or_else(|| {
            ApiError::new(
                ErrorKind::NotFound,
                format!("no vehicle {}", params.vehicle_id),
            )
        })?;
        let now = self.clock.now_us();
        let happened_at_us = checked_time(params.happened_at_us, now)?;
        let entry = HistoryEntry {
            id: format!("hi{}", &self.random.token()[..16]),
            vehicle_id: vehicle.id.clone(),
            kind: EntryKind::Note {
                text: checked_text(&params.text)?,
                category: params.category.map(Into::into),
                tags: checked_tags(params.tags)?,
                earlier: Vec::new(),
            },
            happened_at_us,
            written_at_us: now,
            author: self.author(connection),
            edited_at_us: None,
            deleted: None,
            local: self.kept_local(&vehicle.id),
            version: 1,
        };
        self.commit(
            connection,
            Change::HistoryUpserted {
                entry: entry.clone(),
            },
        );
        Ok(entry)
    }

    /// A note to change: it exists, is a note, is not deleted, and is at
    /// the version the change started from.
    fn note_for_change(&self, id: &str, expected: Option<u64>) -> Result<HistoryEntry, ApiError> {
        let entry =
            self.state.history.get(id).cloned().ok_or_else(|| {
                ApiError::new(ErrorKind::NotFound, format!("no history entry {id}"))
            })?;
        if !entry.is_note() {
            return Err(invalid("UXDuck's own entries cannot be changed"));
        }
        if entry.deleted.is_some() {
            return Err(invalid("that note was deleted"));
        }
        if let Some(expected) = expected
            && expected != entry.version
        {
            return Err(ApiError::new(
                ErrorKind::VersionConflict,
                format!(
                    "the note changed since version {expected}. It is at {}",
                    entry.version
                ),
            ));
        }
        Ok(entry)
    }

    fn edit_note(
        &mut self,
        connection: ConnectionId,
        params: EditNoteParams,
    ) -> Result<HistoryEntry, ApiError> {
        let mut entry = self.note_for_change(&params.id, Some(params.expected_version))?;
        let now = self.clock.now_us();
        let EntryKind::Note {
            text,
            category,
            tags,
            earlier,
        } = &mut entry.kind
        else {
            return Err(invalid("UXDuck's own entries cannot be changed"));
        };
        earlier.push(NoteVersion {
            text: text.clone(),
            category: *category,
            tags: tags.clone(),
            happened_at_us: entry.happened_at_us,
            replaced_at_us: now,
        });
        if let Some(new) = params.text {
            *text = checked_text(&new)?;
        }
        if let Some(new) = params.category {
            *category = Some(new.into());
        }
        if let Some(new) = params.tags {
            *tags = checked_tags(new)?;
        }
        if params.happened_at_us.is_some() {
            entry.happened_at_us = checked_time(params.happened_at_us, now)?;
        }
        entry.edited_at_us = Some(now);
        entry.version += 1;
        self.commit(
            connection,
            Change::HistoryUpserted {
                entry: entry.clone(),
            },
        );
        Ok(entry)
    }

    fn delete_note(
        &mut self,
        connection: ConnectionId,
        id: &str,
    ) -> Result<HistoryEntry, ApiError> {
        let mut entry = self.note_for_change(id, None)?;
        entry.deleted = Some(Deletion {
            by: self.author(connection),
            at_us: self.clock.now_us(),
        });
        entry.version += 1;
        self.commit(
            connection,
            Change::HistoryUpserted {
                entry: entry.clone(),
            },
        );
        Ok(entry)
    }

    /// A vehicle's parameter changes, newest first (S74).
    fn param_history(
        &self,
        params: &uxduck_contracts::dto::history::ParamHistoryParams,
    ) -> Result<uxduck_contracts::dto::history::ParamHistoryResult, ApiError> {
        use uxduck_contracts::dto::history as dto;
        let vehicle =
            crate::param_definitions::queries::which_vehicle(self, params.vehicle_id.as_deref())?;
        let wanted = params.name.as_deref().map(|n| n.trim().to_uppercase());
        let mut changes: Vec<(u64, dto::ParamChange)> = self
            .state
            .history
            .values()
            .filter(|e| belongs(e, &vehicle))
            .filter_map(|e| {
                let EntryKind::Event { event } = &e.kind else {
                    return None;
                };
                let dto::HistoryEvent::ParameterChanged {
                    name,
                    from,
                    to,
                    source,
                    reason,
                    changes,
                    span_us,
                } = dto::HistoryEvent::from(event)
                else {
                    return None;
                };
                if wanted.as_ref().is_some_and(|w| *w != name) {
                    return None;
                }
                Some((
                    e.happened_at_us,
                    dto::ParamChange {
                        name,
                        from,
                        to,
                        source,
                        reason,
                        at_us: e.happened_at_us,
                        by: (&e.author).into(),
                        changes,
                        span_us,
                    },
                ))
            })
            .collect();
        changes.sort_by_key(|c| std::cmp::Reverse(c.0));
        let limit = params.limit.unwrap_or(50).clamp(1, 500) as usize;
        Ok(dto::ParamHistoryResult {
            vehicle_id: vehicle.id,
            changes: changes.into_iter().take(limit).map(|(_, c)| c).collect(),
        })
    }

    fn history_of(&self, params: &VehicleHistoryParams) -> Result<VehicleHistoryResult, ApiError> {
        let vehicle =
            crate::param_definitions::queries::which_vehicle(self, params.vehicle_id.as_deref())?;
        let words: Vec<String> = params
            .text
            .as_deref()
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        let category = params.category.map(Into::into);
        let mut entries: Vec<&HistoryEntry> = self
            .state
            .history
            .values()
            .filter(|e| belongs(e, &vehicle))
            .filter(|e| params.include_deleted || e.deleted.is_none())
            .filter(|e| match params.kind.as_deref() {
                Some("note") => e.is_note(),
                Some("event") => !e.is_note(),
                _ => true,
            })
            .filter(|e| {
                category.is_none()
                    || matches!(&e.kind, EntryKind::Note { category: c, .. } if *c == category)
            })
            .filter(|e| {
                params
                    .since_us
                    .is_none_or(|since| e.happened_at_us >= since)
            })
            .filter(|e| {
                let text = e.text().to_lowercase();
                words.iter().all(|w| text.contains(w.as_str()))
            })
            .collect();
        // Newest first by when it happened, then by when it was written.
        entries.sort_by(|a, b| {
            b.happened_at_us
                .cmp(&a.happened_at_us)
                .then(b.written_at_us.cmp(&a.written_at_us))
        });
        let total = u32::try_from(entries.len()).unwrap_or(u32::MAX);
        let limit = params
            .limit
            .unwrap_or(DEFAULT_ENTRIES)
            .clamp(1, MAX_ENTRIES) as usize;
        Ok(VehicleHistoryResult {
            vehicle_id: vehicle.id.clone(),
            vehicle_name: vehicle.name.clone(),
            entries: entries
                .into_iter()
                .skip(params.offset.unwrap_or(0) as usize)
                .take(limit)
                .map(Into::into)
                .collect(),
            total,
        })
    }
}

fn checked_text(text: &str) -> Result<String, ApiError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(invalid("a note needs some words"));
    }
    if text.chars().count() > MAX_TEXT {
        return Err(invalid(format!("a note has at most {MAX_TEXT} characters")));
    }
    Ok(text.to_owned())
}

fn checked_tags(tags: Vec<String>) -> Result<Vec<String>, ApiError> {
    let mut clean: Vec<String> = tags
        .into_iter()
        .map(|t| t.trim().to_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    clean.dedup();
    if clean.len() > MAX_TAGS || clean.iter().any(|t| t.chars().count() > 32) {
        return Err(invalid(format!(
            "at most {MAX_TAGS} tags of up to 32 characters"
        )));
    }
    Ok(clean)
}

/// When it happened: now, or earlier; never later than now (H1).
fn checked_time(at: Option<u64>, now: u64) -> Result<u64, ApiError> {
    match at {
        None => Ok(now),
        // A minute's grace for clocks a little apart.
        Some(at) if at <= now.saturating_add(60_000_000) => Ok(at.min(now)),
        Some(_) => Err(invalid("a note can be dated earlier, never later than now")),
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
        ClientRequest::VehicleHistory(params) => {
            require(Capability::Read)?;
            to_value(&engine.history_of(&params)?)
        }
        ClientRequest::ParamHistory(params) => {
            require(Capability::Read)?;
            to_value(&engine.param_history(&params)?)
        }
        ClientRequest::VehicleAddNote(params) => {
            require(Capability::Config)?;
            let entry = engine.add_note(connection, params)?;
            to_value(&HistoryEntryDto::from(&entry))
        }
        ClientRequest::VehicleEditNote(params) => {
            require(Capability::Config)?;
            let entry = engine.edit_note(connection, params)?;
            to_value(&HistoryEntryDto::from(&entry))
        }
        ClientRequest::VehicleDeleteNote(IdParams { id }) => {
            require(Capability::Config)?;
            let entry = engine.delete_note(connection, &id)?;
            to_value(&HistoryEntryDto::from(&entry))
        }
        _ => Err(ApiError::new(ErrorKind::Internal, "not a history method")),
    }
}

pub(crate) fn entity_changes(change: &Change) -> Vec<EntityChange> {
    match change {
        Change::HistoryUpserted { entry } => vec![EntityChange {
            history_entry: Some(entry.into()),
            ..upsert(EntityType::HistoryEntry, &entry.id)
        }],
        // Its vehicle goes with it, and with the vehicle its History tab.
        _ => Vec::new(),
    }
}

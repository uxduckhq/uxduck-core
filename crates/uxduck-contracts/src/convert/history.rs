use uxduck_core::domain::history as core;

use crate::dto::history as dto;

impl From<core::NoteCategory> for dto::NoteCategory {
    fn from(v: core::NoteCategory) -> Self {
        match v {
            core::NoteCategory::Maintenance => Self::Maintenance,
            core::NoteCategory::Repair => Self::Repair,
            core::NoteCategory::Damage => Self::Damage,
            core::NoteCategory::Configuration => Self::Configuration,
            core::NoteCategory::Calibration => Self::Calibration,
            core::NoteCategory::Firmware => Self::Firmware,
            core::NoteCategory::Test => Self::Test,
            core::NoteCategory::Other => Self::Other,
        }
    }
}

impl From<dto::NoteCategory> for core::NoteCategory {
    fn from(v: dto::NoteCategory) -> Self {
        match v {
            dto::NoteCategory::Maintenance => Self::Maintenance,
            dto::NoteCategory::Repair => Self::Repair,
            dto::NoteCategory::Damage => Self::Damage,
            dto::NoteCategory::Configuration => Self::Configuration,
            dto::NoteCategory::Calibration => Self::Calibration,
            dto::NoteCategory::Firmware => Self::Firmware,
            dto::NoteCategory::Test => Self::Test,
            dto::NoteCategory::Other => Self::Other,
        }
    }
}

impl From<&core::HistoryEvent> for dto::HistoryEvent {
    fn from(v: &core::HistoryEvent) -> Self {
        match v.clone() {
            core::HistoryEvent::FirstSeen { vehicle_type } => Self::FirstSeen { vehicle_type },
            core::HistoryEvent::Renamed { from, to } => Self::Renamed { from, to },
            core::HistoryEvent::FirmwareChanged { from, to, commit } => {
                Self::FirmwareChanged { from, to, commit }
            }
            core::HistoryEvent::TypeChanged { from, to } => Self::TypeChanged { from, to },
            core::HistoryEvent::SnapshotSaved { snapshot_id, name } => {
                Self::SnapshotSaved { snapshot_id, name }
            }
            core::HistoryEvent::MissionChanged { record_id, text } => {
                Self::MissionChanged { record_id, text }
            }
            core::HistoryEvent::FlightEnded { flight_id, text } => {
                Self::FlightEnded { flight_id, text }
            }
            core::HistoryEvent::ParameterChanged {
                name,
                from,
                to,
                source,
                reason,
                changes,
                span_us,
            } => Self::ParameterChanged {
                name,
                from,
                to,
                source: match source {
                    core::ParamSource::Person => dto::ParamSource::Person,
                    core::ParamSource::Agent => dto::ParamSource::Agent,
                    core::ParamSource::OtherStation => dto::ParamSource::OtherStation,
                    core::ParamSource::Vehicle => dto::ParamSource::Vehicle,
                    core::ParamSource::Download => dto::ParamSource::Download,
                },
                reason,
                changes,
                span_us,
            },
            core::HistoryEvent::SupervisionLost { failsafe } => Self::SupervisionLost { failsafe },
            core::HistoryEvent::SupervisionResumed => Self::SupervisionResumed,
        }
    }
}

impl From<&core::Author> for dto::Author {
    fn from(v: &core::Author) -> Self {
        Self {
            client_name: v.client_name.clone(),
            client_kind: v.client_kind.clone(),
            operator_id: v.operator_id.clone(),
            operator_name: v.operator_name.clone(),
        }
    }
}

impl From<&core::HistoryEntry> for dto::HistoryEntry {
    fn from(v: &core::HistoryEntry) -> Self {
        Self {
            id: v.id.clone(),
            vehicle_id: v.vehicle_id.clone(),
            kind: match &v.kind {
                core::EntryKind::Note {
                    text,
                    category,
                    tags,
                    earlier,
                } => dto::EntryKind::Note {
                    text: text.clone(),
                    category: category.map(Into::into),
                    tags: tags.clone(),
                    earlier: earlier
                        .iter()
                        .map(|e| dto::NoteVersion {
                            text: e.text.clone(),
                            category: e.category.map(Into::into),
                            tags: e.tags.clone(),
                            happened_at_us: e.happened_at_us,
                            replaced_at_us: e.replaced_at_us,
                        })
                        .collect(),
                },
                core::EntryKind::Event { event } => dto::EntryKind::Event {
                    event: event.into(),
                },
            },
            happened_at_us: v.happened_at_us,
            written_at_us: v.written_at_us,
            author: (&v.author).into(),
            edited_at_us: v.edited_at_us,
            deleted: v.deleted.as_ref().map(|d| dto::Deletion {
                by: (&d.by).into(),
                at_us: d.at_us,
            }),
            local: v.local,
            version: v.version,
        }
    }
}

use uxduck_core::domain::dialect as core;

use crate::dto::dialect as dto;

impl From<core::MessageChange> for dto::MessageChange {
    fn from(v: core::MessageChange) -> Self {
        match v {
            core::MessageChange::New => Self::New,
            core::MessageChange::Layout => Self::Layout,
            core::MessageChange::Extensions => Self::Extensions,
        }
    }
}

impl From<&core::CustomField> for dto::DialectField {
    fn from(v: &core::CustomField) -> Self {
        Self {
            name: v.name.clone(),
            kind: v.kind.name().to_owned(),
            length: v.length,
            extension: v.extension,
            units: v.units.clone(),
            enum_name: v.enum_name.clone(),
            description: v.description.clone(),
        }
    }
}

impl From<&core::CustomMessage> for dto::DialectMessage {
    fn from(v: &core::CustomMessage) -> Self {
        Self {
            id: v.id,
            name: v.name.clone(),
            change: v.change.into(),
            crc_extra: v.crc_extra,
            description: v.description.clone(),
            fields: v.fields.iter().map(Into::into).collect(),
        }
    }
}

impl From<&core::CustomEnum> for dto::DialectEnum {
    fn from(v: &core::CustomEnum) -> Self {
        Self {
            name: v.name.clone(),
            bitmask: v.bitmask,
            entries: v
                .entries
                .iter()
                .map(|e| dto::DialectEnumEntry {
                    value: e.value,
                    name: e.name.clone(),
                })
                .collect(),
        }
    }
}

impl From<&core::CustomCommand> for dto::DialectCommand {
    fn from(v: &core::CustomCommand) -> Self {
        Self {
            value: v.value,
            name: v.name.clone(),
            description: v.description.clone(),
            has_location: v.has_location,
            no_location: v.no_location,
            params: v
                .params
                .iter()
                .map(|p| dto::DialectCommandParam {
                    index: p.index,
                    label: p.label.clone(),
                    units: p.units.clone(),
                    description: p.description.clone(),
                })
                .collect(),
        }
    }
}

impl From<&core::Dialect> for dto::Dialect {
    /// Without `unread`, `used_by` and `add_again`, which the engine knows.
    fn from(v: &core::Dialect) -> Self {
        Self {
            id: v.id.clone(),
            label: v.label.clone(),
            files: v.files.clone(),
            commits: v.commits.clone(),
            every_vehicle: v.every_vehicle,
            used_by: Vec::new(),
            messages: v.messages.iter().map(Into::into).collect(),
            enums: v.enums.iter().map(Into::into).collect(),
            commands: v.commands.iter().map(Into::into).collect(),
            unchanged: v.unchanged,
            added_at_us: v.added_at_us,
            unread: Vec::new(),
            profile_id: v.profile_id.clone(),
            add_again: false,
        }
    }
}

impl From<&core::DialectChoice> for dto::DialectChoice {
    fn from(v: &core::DialectChoice) -> Self {
        match v {
            core::DialectChoice::Automatic => Self::Automatic,
            core::DialectChoice::Chosen { ids } => Self::Chosen { ids: ids.clone() },
        }
    }
}

impl From<dto::DialectChoice> for core::DialectChoice {
    fn from(v: dto::DialectChoice) -> Self {
        match v {
            dto::DialectChoice::Automatic => Self::Automatic,
            dto::DialectChoice::Chosen { ids } => Self::Chosen { ids },
        }
    }
}

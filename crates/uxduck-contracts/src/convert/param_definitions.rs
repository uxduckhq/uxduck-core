use uxduck_core::domain::param_definitions as core;

use crate::dto::param_definitions as dto;

impl From<core::Firmware> for dto::Firmware {
    fn from(v: core::Firmware) -> Self {
        match v {
            core::Firmware::ArduCopter => Self::ArduCopter,
            core::Firmware::ArduPlane => Self::ArduPlane,
            core::Firmware::Rover => Self::Rover,
            core::Firmware::ArduSub => Self::ArduSub,
            core::Firmware::AntennaTracker => Self::AntennaTracker,
            core::Firmware::Blimp => Self::Blimp,
        }
    }
}

impl From<dto::Firmware> for core::Firmware {
    fn from(v: dto::Firmware) -> Self {
        match v {
            dto::Firmware::ArduCopter => Self::ArduCopter,
            dto::Firmware::ArduPlane => Self::ArduPlane,
            dto::Firmware::Rover => Self::Rover,
            dto::Firmware::ArduSub => Self::ArduSub,
            dto::Firmware::AntennaTracker => Self::AntennaTracker,
            dto::Firmware::Blimp => Self::Blimp,
        }
    }
}

impl From<&core::DefinitionSource> for dto::DefinitionSource {
    fn from(v: &core::DefinitionSource) -> Self {
        match v {
            core::DefinitionSource::Ardupilot => Self::Ardupilot,
            core::DefinitionSource::Own { label } => Self::Own {
                label: label.clone(),
            },
        }
    }
}

impl From<dto::DefinitionSource> for core::DefinitionSource {
    fn from(v: dto::DefinitionSource) -> Self {
        match v {
            dto::DefinitionSource::Ardupilot => Self::Ardupilot,
            dto::DefinitionSource::Own { label } => Self::Own { label },
        }
    }
}

const fn user_out(v: core::UserLevel) -> dto::UserLevel {
    match v {
        core::UserLevel::Standard => dto::UserLevel::Standard,
        core::UserLevel::Advanced => dto::UserLevel::Advanced,
    }
}

const fn user_in(v: dto::UserLevel) -> core::UserLevel {
    match v {
        dto::UserLevel::Standard => core::UserLevel::Standard,
        dto::UserLevel::Advanced => core::UserLevel::Advanced,
    }
}

impl From<&core::ParamDefinition> for dto::ParamDefinition {
    fn from(v: &core::ParamDefinition) -> Self {
        Self {
            name: v.name.clone(),
            display_name: v.display_name.clone(),
            description: v.description.clone(),
            group: v.group.clone(),
            units: v.units.clone(),
            range: v.range.map(|r| dto::ParamRange {
                low: r.low,
                high: r.high,
            }),
            increment: v.increment,
            values: v
                .values
                .iter()
                .map(|n| dto::ValueName {
                    value: n.value,
                    name: n.name.clone(),
                })
                .collect(),
            bitmask: v
                .bitmask
                .iter()
                .map(|b| dto::BitName {
                    bit: b.bit,
                    name: b.name.clone(),
                })
                .collect(),
            user: user_out(v.user),
            reboot_required: v.reboot_required,
            read_only: v.read_only,
            volatile: v.volatile,
            calibration: v.calibration,
            variant: v.variant.clone(),
        }
    }
}

impl From<dto::ParamDefinition> for core::ParamDefinition {
    fn from(v: dto::ParamDefinition) -> Self {
        Self {
            name: v.name,
            display_name: v.display_name,
            description: v.description,
            group: v.group,
            units: v.units,
            range: v.range.map(|r| core::ParamRange {
                low: r.low,
                high: r.high,
            }),
            increment: v.increment,
            values: v
                .values
                .into_iter()
                .map(|n| core::ValueName {
                    value: n.value,
                    name: n.name,
                })
                .collect(),
            bitmask: v
                .bitmask
                .into_iter()
                .map(|b| core::BitName {
                    bit: b.bit,
                    name: b.name,
                })
                .collect(),
            user: user_in(v.user),
            reboot_required: v.reboot_required,
            read_only: v.read_only,
            volatile: v.volatile,
            calibration: v.calibration,
            variant: v.variant,
        }
    }
}

impl From<&core::DefinitionSet> for dto::DefinitionSet {
    fn from(v: &core::DefinitionSet) -> Self {
        Self {
            source: (&v.source).into(),
            firmware: v.firmware.into(),
            version: v.version.clone(),
            commit: v.commit.clone(),
            parameters: v.parameters.iter().map(Into::into).collect(),
        }
    }
}

impl From<dto::DefinitionSet> for core::DefinitionSet {
    fn from(v: dto::DefinitionSet) -> Self {
        let mut set = Self {
            source: v.source.into(),
            firmware: v.firmware.into(),
            version: v.version,
            commit: v.commit,
            parameters: v.parameters.into_iter().map(Into::into).collect(),
        };
        set.sort();
        set
    }
}

impl From<core::Match> for dto::Match {
    fn from(v: core::Match) -> Self {
        match v {
            core::Match::Own => Self::Own,
            core::Match::Commit => Self::Commit,
            core::Match::Exact => Self::Exact,
            core::Match::Approximate => Self::Approximate,
        }
    }
}

impl From<&core::VehicleDefinitions> for dto::VehicleDefinitions {
    fn from(v: &core::VehicleDefinitions) -> Self {
        Self {
            vehicle_id: v.vehicle_id.clone(),
            status: match v.status {
                core::DefinitionsStatus::Loading => dto::DefinitionsStatus::Loading,
                core::DefinitionsStatus::Ready => dto::DefinitionsStatus::Ready,
                core::DefinitionsStatus::Missing => dto::DefinitionsStatus::Missing,
                core::DefinitionsStatus::Failed => dto::DefinitionsStatus::Failed,
            },
            set_id: v.set_id.clone(),
            title: v.title.clone(),
            matched: v.matched.map(Into::into),
            detail: v.detail.clone(),
        }
    }
}

impl From<&core::OwnSet> for dto::OwnSet {
    fn from(v: &core::OwnSet) -> Self {
        Self {
            id: v.id.clone(),
            label: v.label.clone(),
            firmware: v.firmware.into(),
            version: v.version.clone(),
            commit: v.commit.clone(),
            vehicle_id: v.vehicle_id.clone(),
            parameter_count: v.parameter_count,
            added_at_us: v.added_at_us,
            url: v.url.clone(),
            fetched_at_us: v.fetched_at_us,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_and_the_file_are_the_same_json() {
        let (_, parameters) =
            crate::pdef::from_json(include_str!("../../fixtures/pdef/copter.pdef.json")).unwrap();
        let set = core::DefinitionSet {
            source: core::DefinitionSource::Ardupilot,
            firmware: core::Firmware::ArduCopter,
            version: Some("4.5.7".into()),
            commit: None,
            parameters,
        };
        let wire = dto::DefinitionSet::from(&set);
        assert_eq!(
            serde_json::to_value(&wire).unwrap(),
            serde_json::to_value(&set).unwrap()
        );
        assert_eq!(core::DefinitionSet::from(wire), set);
    }
}

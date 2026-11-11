use uxduck_core::domain::firmware_profile as core;

use crate::dto::firmware_profile as dto;

impl From<&core::FirmwareProfile> for dto::FirmwareProfile {
    /// Without `problem`, which the engine knows.
    fn from(v: &core::FirmwareProfile) -> Self {
        Self {
            id: v.id.clone(),
            url: v.url.clone(),
            label: v.label.clone(),
            firmware: v.firmware.map(Into::into),
            version: v.version.clone(),
            commit: v.commit.clone(),
            own_set_id: v.own_set_id.clone(),
            dialect_id: v.dialect_id.clone(),
            added_at_us: v.added_at_us,
            fetched_at_us: v.fetched_at_us,
            problem: None,
        }
    }
}

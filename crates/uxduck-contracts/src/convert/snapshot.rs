use uxduck_core::domain::snapshot as s;

use crate::dto::sim as dto;

impl From<&s::Snapshot> for dto::SimSnapshot {
    fn from(v: &s::Snapshot) -> Self {
        Self {
            id: v.id.clone(),
            name: v.name.clone(),
            vehicle: v.vehicle.into(),
            autopilot: v.autopilot.into(),
            frame: v.frame.clone(),
            firmware: v.firmware.clone(),
            latitude_deg: v.latitude_deg,
            longitude_deg: v.longitude_deg,
            ground_altitude_m: v.ground_altitude_m,
            height_m: v.height_m,
            heading_deg: v.heading_deg,
            ground_speed_mps: v.ground_speed_mps,
            mode: v.mode.clone(),
            armed: v.armed,
            flying: v.flying(),
            parameter_count: u32::try_from(v.parameters.len()).unwrap_or(u32::MAX),
            mission_count: u32::try_from(v.mission.len()).unwrap_or(u32::MAX),
            current_item: v.current_item,
            operator_id: v.operator_id.clone(),
            created_at_us: v.created_at_us,
            version: v.version,
        }
    }
}

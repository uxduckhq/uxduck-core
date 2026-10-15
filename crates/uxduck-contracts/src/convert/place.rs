use uxduck_core::domain::place as p;

use crate::dto::search as dto;

impl From<&p::SavedPlace> for dto::SavedPlace {
    fn from(v: &p::SavedPlace) -> Self {
        Self {
            id: v.id.clone(),
            operator_id: v.operator_id.clone(),
            name: v.name.clone(),
            latitude_deg: v.latitude_deg,
            longitude_deg: v.longitude_deg,
            created_at_us: v.created_at_us,
            version: v.version,
        }
    }
}

use uxduck_core::domain::mission as m;
use uxduck_core::domain::parameter as p;

use crate::dto::mission as dm;
use crate::dto::parameter as dto;

impl From<p::ParamKind> for dto::ParamKind {
    fn from(v: p::ParamKind) -> Self {
        match v {
            p::ParamKind::Uint8 => Self::Uint8,
            p::ParamKind::Int8 => Self::Int8,
            p::ParamKind::Uint16 => Self::Uint16,
            p::ParamKind::Int16 => Self::Int16,
            p::ParamKind::Uint32 => Self::Uint32,
            p::ParamKind::Int32 => Self::Int32,
            p::ParamKind::Uint64 => Self::Uint64,
            p::ParamKind::Int64 => Self::Int64,
            p::ParamKind::Real32 => Self::Real32,
            p::ParamKind::Real64 => Self::Real64,
        }
    }
}

impl From<&p::Parameter> for dto::Parameter {
    fn from(v: &p::Parameter) -> Self {
        Self {
            id: v.id(),
            vehicle_id: v.vehicle_id.clone(),
            name: v.name.clone(),
            value: v.value,
            kind: v.kind.into(),
            default: v.default,
        }
    }
}

impl From<p::TransferStatus> for dto::TransferStatus {
    fn from(v: p::TransferStatus) -> Self {
        match v {
            p::TransferStatus::Unknown => Self::Unknown,
            p::TransferStatus::Downloading => Self::Downloading,
            p::TransferStatus::Uploading => Self::Uploading,
            p::TransferStatus::Synced => Self::Synced,
            p::TransferStatus::Failed => Self::Failed,
        }
    }
}

impl From<&p::ParameterSet> for dto::ParameterSet {
    fn from(v: &p::ParameterSet) -> Self {
        Self {
            vehicle_id: v.vehicle_id.clone(),
            status: v.status.into(),
            detail: v.detail.clone(),
            count: v.count,
            boot_instant_us: v.boot_instant_us,
            synced_at_us: v.synced_at_us,
            progress: v.progress.map(progress),
        }
    }
}

fn progress(v: p::TransferProgress) -> dto::TransferProgress {
    dto::TransferProgress {
        done: v.done,
        total: v.total,
        attempt: v.attempt,
        at_us: v.at_us,
        upload: v.upload,
    }
}

impl From<&m::MissionItem> for dm::MissionItem {
    fn from(v: &m::MissionItem) -> Self {
        Self {
            command: v.command,
            frame: v.frame,
            param1: v.param1,
            param2: v.param2,
            param3: v.param3,
            param4: v.param4,
            latitude_deg: v.latitude_deg,
            longitude_deg: v.longitude_deg,
            altitude_m: v.altitude_m,
            autocontinue: v.autocontinue,
        }
    }
}

impl From<dm::MissionItem> for m::MissionItem {
    fn from(v: dm::MissionItem) -> Self {
        Self {
            command: v.command,
            frame: v.frame,
            param1: v.param1,
            param2: v.param2,
            param3: v.param3,
            param4: v.param4,
            latitude_deg: v.latitude_deg,
            longitude_deg: v.longitude_deg,
            altitude_m: v.altitude_m,
            autocontinue: v.autocontinue,
        }
    }
}

impl From<&m::Mission> for dm::Mission {
    fn from(v: &m::Mission) -> Self {
        Self {
            vehicle_id: v.vehicle_id.clone(),
            items: v.items.iter().map(Into::into).collect(),
            status: v.status.into(),
            detail: v.detail.clone(),
            boot_instant_us: v.boot_instant_us,
            synced_at_us: v.synced_at_us,
            version: v.version,
            progress: v.progress.map(progress),
            fence: v.fence.iter().map(Into::into).collect(),
            rally: v.rally.iter().map(Into::into).collect(),
        }
    }
}

impl From<&m::MissionOrigin> for dm::MissionOrigin {
    fn from(v: &m::MissionOrigin) -> Self {
        match v {
            m::MissionOrigin::Found => Self::Found,
            m::MissionOrigin::Uploaded {
                plan_id,
                plan_name,
                verified,
            } => Self::Uploaded {
                plan_id: plan_id.clone(),
                plan_name: plan_name.clone(),
                verified: *verified,
            },
            m::MissionOrigin::Example => Self::Example,
            m::MissionOrigin::ChangedElsewhere => Self::ChangedElsewhere,
        }
    }
}

impl From<&m::MissionRecord> for dm::MissionRecord {
    fn from(v: &m::MissionRecord) -> Self {
        Self {
            id: v.id.clone(),
            vehicle_id: v.vehicle_id.clone(),
            items: v.items.iter().map(Into::into).collect(),
            fence: v.fence.iter().map(Into::into).collect(),
            rally: v.rally.iter().map(Into::into).collect(),
            origin: (&v.origin).into(),
            origin_text: v.origin_words(),
            counts: v.counts(),
            client_id: v.client_id.clone(),
            operator_id: v.operator_id.clone(),
            first_seen_us: v.first_seen_us,
            last_seen_us: v.last_seen_us,
            times: v.times,
        }
    }
}

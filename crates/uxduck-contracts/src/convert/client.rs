use uxduck_core::domain::approval as a;
use uxduck_core::domain::client as c;

use crate::dto::client as dto;

impl From<&c::ClientRecord> for dto::ClientRecord {
    fn from(v: &c::ClientRecord) -> Self {
        Self {
            id: v.id.clone(),
            name: v.name.clone(),
            kind: v.kind.into(),
            executable: v.executable.clone(),
            capabilities: v.capabilities.iter().copied().map(Into::into).collect(),
            created_at_us: v.created_at_us,
            version: v.version,
        }
    }
}

impl From<a::Remember> for dto::Remember {
    fn from(v: a::Remember) -> Self {
        match v {
            a::Remember::Once => Self::Once,
            a::Remember::Connection => Self::Connection,
            a::Remember::Always => Self::Always,
        }
    }
}

impl From<dto::Remember> for a::Remember {
    fn from(v: dto::Remember) -> Self {
        match v {
            dto::Remember::Once => Self::Once,
            dto::Remember::Connection => Self::Connection,
            dto::Remember::Always => Self::Always,
        }
    }
}

impl From<&a::Approval> for dto::Approval {
    fn from(v: &a::Approval) -> Self {
        Self {
            id: v.id.clone(),
            client_id: v.client_id.clone(),
            client_name: v.client_name.clone(),
            capability: v.capability.into(),
            method: v.method.clone(),
            summary: v.summary.clone(),
            preflight: v.preflight.iter().map(Into::into).collect(),
            state: match v.state {
                a::ApprovalState::Pending => dto::ApprovalState::Pending,
                a::ApprovalState::Allowed => dto::ApprovalState::Allowed,
                a::ApprovalState::Denied => dto::ApprovalState::Denied,
                a::ApprovalState::Expired => dto::ApprovalState::Expired,
            },
            remember: v.remember.map(Into::into),
            deadline_us: v.deadline_us,
            created_at_us: v.created_at_us,
            version: v.version,
        }
    }
}

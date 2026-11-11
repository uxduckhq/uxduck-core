use uxduck_core::domain::companion as c;

use crate::dto::companion as dto;

impl From<c::CompanionRole> for dto::CompanionRole {
    fn from(v: c::CompanionRole) -> Self {
        match v {
            c::CompanionRole::Forward => Self::Forward,
            c::CompanionRole::Report => Self::Report,
        }
    }
}

impl From<dto::CompanionRole> for c::CompanionRole {
    fn from(v: dto::CompanionRole) -> Self {
        match v {
            dto::CompanionRole::Forward => Self::Forward,
            dto::CompanionRole::Report => Self::Report,
        }
    }
}

impl From<&c::CompanionComputer> for dto::CompanionComputer {
    fn from(v: &c::CompanionComputer) -> Self {
        Self {
            id: v.id.clone(),
            name: v.name.clone(),
            host: v.host.clone(),
            user: v.user.clone(),
            port: v.port,
            role: v.role.into(),
            component_id: v.component_id,
            key: v.key,
            check: v.check.as_ref().map(|check| dto::CompanionCheck {
                state: match check.state {
                    c::CheckState::Checking => dto::CheckState::Checking,
                    c::CheckState::Reached => dto::CheckState::Reached,
                    c::CheckState::Failed => dto::CheckState::Failed,
                },
                at_us: check.at_us,
                os: check.os.clone(),
                arch: check.arch.clone(),
                uxduck: check.uxduck.clone(),
                host_key: check.host_key.clone(),
                problem: check.problem.clone(),
            }),
        }
    }
}

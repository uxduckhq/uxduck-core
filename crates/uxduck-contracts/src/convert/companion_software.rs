use uxduck_core::domain::companion_software as c;

use crate::dto::companion_software as dto;

impl From<&c::FoundUnit> for dto::FoundUnit {
    fn from(v: &c::FoundUnit) -> Self {
        Self {
            name: v.name.clone(),
            own: v.own,
            user: v.user.clone(),
            drop_ins: v.drop_ins,
            exec: v.exec.clone(),
        }
    }
}

impl From<&c::SoftwareFound> for dto::SoftwareFound {
    fn from(v: &c::SoftwareFound) -> Self {
        Self {
            link: v.link.clone(),
            kept: v.kept.clone(),
            config: v.config,
            uses: v.uses.clone(),
            has: v.has.clone(),
            units: v.units.iter().map(Into::into).collect(),
            sudo: v.sudo,
            serial: v.serial.clone(),
            systemd: v.systemd,
            bits: v.bits,
        }
    }
}

impl From<c::InstallAction> for dto::InstallAction {
    fn from(v: c::InstallAction) -> Self {
        match v {
            c::InstallAction::Install => Self::Install,
            c::InstallAction::Update => Self::Update,
            c::InstallAction::Remove => Self::Remove,
        }
    }
}

impl From<dto::InstallAction> for c::InstallAction {
    fn from(v: dto::InstallAction) -> Self {
        match v {
            dto::InstallAction::Install => Self::Install,
            dto::InstallAction::Update => Self::Update,
            dto::InstallAction::Remove => Self::Remove,
        }
    }
}

impl From<&c::InstallRecord> for dto::InstallRecord {
    fn from(v: &c::InstallRecord) -> Self {
        Self {
            action: v.action.into(),
            state: match v.state {
                c::InstallState::Running => dto::InstallState::Running,
                c::InstallState::Done => dto::InstallState::Done,
                c::InstallState::RolledBack => dto::InstallState::RolledBack,
                c::InstallState::Failed => dto::InstallState::Failed,
            },
            at_us: v.at_us,
            version: v.version.clone(),
            steps: v
                .steps
                .iter()
                .map(|s| dto::InstallStep {
                    title: s.title.clone(),
                    ok: s.ok,
                    output: s.output.clone(),
                })
                .collect(),
            problem: v.problem.clone(),
        }
    }
}

impl From<c::SoftwareStatus> for dto::SoftwareStatus {
    fn from(v: c::SoftwareStatus) -> Self {
        match v {
            c::SoftwareStatus::NotChecked => Self::NotChecked,
            c::SoftwareStatus::Checking => Self::Checking,
            c::SoftwareStatus::Unreachable => Self::Unreachable,
            c::SoftwareStatus::NotInstalled => Self::NotInstalled,
            c::SoftwareStatus::UpToDate => Self::UpToDate,
            c::SoftwareStatus::UpdateAvailable => Self::UpdateAvailable,
            c::SoftwareStatus::UpdateNeeded => Self::UpdateNeeded,
            c::SoftwareStatus::Newer => Self::Newer,
            c::SoftwareStatus::Unsupported => Self::Unsupported,
            c::SoftwareStatus::Unknown => Self::Unknown,
        }
    }
}

impl From<c::Plan> for dto::InstallPlan {
    fn from(v: c::Plan) -> Self {
        Self {
            action: v.action.into(),
            version: v.version,
            target: v.target,
            user: c::USER.to_owned(),
            steps: v.steps,
            keeps: v.keeps,
            warnings: v.warnings,
            mavlink_pauses: v.mavlink_pauses,
            unit: v.unit,
            polkit: v.polkit,
            groups: v.groups,
            replaces: v.replaces,
        }
    }
}

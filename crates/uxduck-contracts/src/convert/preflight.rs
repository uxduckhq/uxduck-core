use uxduck_core::domain::preflight as p;

use crate::dto::preflight as dto;

impl From<p::CheckSource> for dto::CheckSource {
    fn from(v: p::CheckSource) -> Self {
        match v {
            p::CheckSource::Vehicle => Self::Vehicle,
            p::CheckSource::Uxduck => Self::Uxduck,
            p::CheckSource::Checklist => Self::Checklist,
            p::CheckSource::Plugin => Self::Plugin,
        }
    }
}

impl From<p::CheckSeverity> for dto::CheckSeverity {
    fn from(v: p::CheckSeverity) -> Self {
        match v {
            p::CheckSeverity::Block => Self::Block,
            p::CheckSeverity::Warn => Self::Warn,
        }
    }
}

impl From<dto::CheckSeverity> for p::CheckSeverity {
    fn from(v: dto::CheckSeverity) -> Self {
        match v {
            dto::CheckSeverity::Block => Self::Block,
            dto::CheckSeverity::Warn => Self::Warn,
        }
    }
}

impl From<p::CheckWhen> for dto::CheckWhen {
    fn from(v: p::CheckWhen) -> Self {
        match v {
            p::CheckWhen::Preflight => Self::Preflight,
            p::CheckWhen::Launch => Self::Launch,
        }
    }
}

impl From<dto::CheckWhen> for p::CheckWhen {
    fn from(v: dto::CheckWhen) -> Self {
        match v {
            dto::CheckWhen::Preflight => Self::Preflight,
            dto::CheckWhen::Launch => Self::Launch,
        }
    }
}

impl From<p::InputPath> for dto::InputPath {
    fn from(v: p::InputPath) -> Self {
        match v {
            p::InputPath::Either => Self::Either,
            p::InputPath::Rc => Self::Rc,
            p::InputPath::Link => Self::Link,
        }
    }
}

impl From<dto::InputPath> for p::InputPath {
    fn from(v: dto::InputPath) -> Self {
        match v {
            dto::InputPath::Either => Self::Either,
            dto::InputPath::Rc => Self::Rc,
            dto::InputPath::Link => Self::Link,
        }
    }
}

impl From<p::CheckStatus> for dto::CheckStatus {
    fn from(v: p::CheckStatus) -> Self {
        match v {
            p::CheckStatus::Pass => Self::Pass,
            p::CheckStatus::Fail => Self::Fail,
            p::CheckStatus::Waiting => Self::Waiting,
            p::CheckStatus::Unknown => Self::Unknown,
            p::CheckStatus::ToDo => Self::ToDo,
        }
    }
}

impl From<&p::CheckFix> for dto::CheckFix {
    fn from(v: &p::CheckFix) -> Self {
        match v {
            p::CheckFix::Setup { row } => Self::Setup { row: (*row).into() },
            p::CheckFix::Plan { plan_id } => Self::Plan {
                plan_id: plan_id.clone(),
            },
            p::CheckFix::Plans => Self::Plans,
            p::CheckFix::Mission => Self::Mission,
            p::CheckFix::Controllers => Self::Controllers,
            p::CheckFix::Cameras => Self::Cameras,
            p::CheckFix::Restart => Self::Restart,
            p::CheckFix::Messages => Self::Messages,
            p::CheckFix::Connections => Self::Connections,
            p::CheckFix::Plugin { plugin_id } => Self::Plugin {
                plugin_id: plugin_id.clone(),
            },
            p::CheckFix::Home => Self::Home,
        }
    }
}

impl From<p::OwnCheck> for dto::OwnCheck {
    fn from(v: p::OwnCheck) -> Self {
        match v {
            p::OwnCheck::Setup => Self::Setup,
            p::OwnCheck::Restart => Self::Restart,
            p::OwnCheck::Link => Self::Link,
            p::OwnCheck::RcReceiver => Self::RcReceiver,
            p::OwnCheck::Controller => Self::Controller,
            p::OwnCheck::Plan => Self::Plan,
            p::OwnCheck::Terrain => Self::Terrain,
            p::OwnCheck::Recording => Self::Recording,
            p::OwnCheck::Home => Self::Home,
            p::OwnCheck::Finished => Self::PreflightFinished,
        }
    }
}

impl From<dto::OwnCheck> for p::OwnCheck {
    fn from(v: dto::OwnCheck) -> Self {
        match v {
            dto::OwnCheck::Setup => Self::Setup,
            dto::OwnCheck::Restart => Self::Restart,
            dto::OwnCheck::Link => Self::Link,
            dto::OwnCheck::RcReceiver => Self::RcReceiver,
            dto::OwnCheck::Controller => Self::Controller,
            dto::OwnCheck::Plan => Self::Plan,
            dto::OwnCheck::Terrain => Self::Terrain,
            dto::OwnCheck::Recording => Self::Recording,
            dto::OwnCheck::Home => Self::Home,
            dto::OwnCheck::PreflightFinished => Self::Finished,
        }
    }
}

impl From<&p::Check> for dto::Check {
    fn from(v: &p::Check) -> Self {
        Self {
            id: v.id.clone(),
            source: v.source.into(),
            title: v.title.clone(),
            group: v.group.clone(),
            severity: v.severity.into(),
            when: v.when.map(Into::into),
            status: v.status.into(),
            detail: v.detail.clone(),
            fix: v.fix.as_ref().map(Into::into),
            gates: v.gates(),
            plugin: v.plugin.as_ref().map(|p| dto::CheckPlugin {
                id: p.id.clone(),
                name: p.name.clone(),
                held_back: p.held_back,
            }),
        }
    }
}

impl From<&p::CheckRef> for dto::CheckRef {
    fn from(v: &p::CheckRef) -> Self {
        Self {
            id: v.id.clone(),
            title: v.title.clone(),
            status: v.status.into(),
            detail: v.detail.clone(),
        }
    }
}

impl From<&p::PreflightOverride> for dto::PreflightOverride {
    fn from(v: &p::PreflightOverride) -> Self {
        match v {
            p::PreflightOverride::Operator => Self::Operator,
            p::PreflightOverride::Approval { approval_id } => Self::Approval {
                approval_id: approval_id.clone(),
            },
        }
    }
}

/// Each of UXDuck's own checks as set for a vehicle, on or off.
#[must_use]
pub fn own_settings(settings: &p::PreflightSettings) -> Vec<dto::OwnCheckSetting> {
    p::OwnCheck::ALL
        .iter()
        .map(|&check| {
            let setting = settings.setting(check);
            dto::OwnCheckSetting {
                check: check.into(),
                id: check.id(),
                title: check.title().to_owned(),
                group: settings.group(check).to_owned(),
                on: setting.on,
                severity: setting.severity.into(),
                default_severity: check.default_severity().into(),
                threshold: check.threshold().map(|meta| dto::CheckThreshold {
                    value: settings.threshold(check).unwrap_or(meta.default),
                    default: meta.default,
                    min: meta.min,
                    max: meta.max,
                    unit: meta.unit.to_owned(),
                    label: meta.label.to_owned(),
                }),
                input: settings.input(check).map(Into::into),
                when: check.when().map(Into::into),
            }
        })
        .collect()
}

impl From<&p::Preflight> for dto::Preflight {
    fn from(v: &p::Preflight) -> Self {
        Self {
            vehicle_id: v.vehicle_id.clone(),
            checks: v.checks.iter().map(Into::into).collect(),
            settings: own_settings(&v.settings),
            groups: v
                .settings
                .groups_shown(v.checklist.as_ref().map_or(&[], |c| &c.steps)),
            plan_id: v.plan_id.clone(),
            evaluating: v.evaluating,
            checklist: crate::dto::checklist::Checklist {
                steps: v
                    .checklist
                    .iter()
                    .flat_map(|c| c.steps.iter())
                    .map(|s| super::checklist::step(s, v.flown_since.contains(&s.id)))
                    .collect(),
                copied_from: v.checklist.as_ref().and_then(|c| c.copied_from.clone()),
                offers: v.offers.iter().map(Into::into).collect(),
                run: v
                    .checklist
                    .as_ref()
                    .and_then(|c| c.run.as_ref())
                    .map(Into::into),
                version: v.checklist.as_ref().map_or(0, |c| c.version),
            },
            updated_at_us: v.updated_at_us,
            version: v.version,
        }
    }
}

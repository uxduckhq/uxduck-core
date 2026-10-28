use uxduck_core::domain::checklist as c;
use uxduck_core::domain::preflight::CheckSeverity;

use crate::dto::checklist as dto;

impl From<c::Op> for dto::RuleOp {
    fn from(v: c::Op) -> Self {
        match v {
            c::Op::Ge => Self::Ge,
            c::Op::Le => Self::Le,
            c::Op::Gt => Self::Gt,
            c::Op::Lt => Self::Lt,
            c::Op::Eq => Self::Eq,
            c::Op::Ne => Self::Ne,
        }
    }
}

impl From<dto::RuleOp> for c::Op {
    fn from(v: dto::RuleOp) -> Self {
        match v {
            dto::RuleOp::Ge => Self::Ge,
            dto::RuleOp::Le => Self::Le,
            dto::RuleOp::Gt => Self::Gt,
            dto::RuleOp::Lt => Self::Lt,
            dto::RuleOp::Eq => Self::Eq,
            dto::RuleOp::Ne => Self::Ne,
        }
    }
}

impl From<c::StepKind> for dto::StepKind {
    fn from(v: c::StepKind) -> Self {
        match v {
            c::StepKind::Manual => Self::Manual,
            c::StepKind::Rule => Self::Rule,
        }
    }
}

impl From<dto::StepKind> for c::StepKind {
    fn from(v: dto::StepKind) -> Self {
        match v {
            dto::StepKind::Manual => Self::Manual,
            dto::StepKind::Rule => Self::Rule,
        }
    }
}

impl From<&c::Tick> for dto::Tick {
    fn from(v: &c::Tick) -> Self {
        Self {
            by: (&v.by).into(),
            at_us: v.at_us,
        }
    }
}

/// A step as written, before it is checked: a rule needs its operator and
/// value, which the check then asks for in words.
impl From<dto::StepInput> for c::StepDef {
    fn from(v: dto::StepInput) -> Self {
        let kind: c::StepKind = v.kind.into();
        let rule = (kind == c::StepKind::Rule
            || v.item.is_some()
            || v.param.is_some()
            || v.op.is_some()
            || v.value.is_some())
        .then(|| c::Rule {
            item: v.item,
            param: v.param,
            op: v.op.map_or(c::Op::Ge, Into::into),
            value: v.value.unwrap_or(f64::NAN),
            unit: v.unit,
            hold_s: v.hold_s,
        });
        Self {
            kind,
            title: v.title,
            severity: v.severity.map_or(CheckSeverity::Block, Into::into),
            rule,
        }
    }
}

impl From<&c::StepDef> for dto::StepInput {
    fn from(v: &c::StepDef) -> Self {
        let rule = v.rule.as_ref();
        Self {
            kind: v.kind.into(),
            title: v.title.clone(),
            severity: Some(v.severity.into()),
            item: rule.and_then(|r| r.item.clone()),
            param: rule.and_then(|r| r.param.clone()),
            op: rule.map(|r| r.op.into()),
            value: rule.map(|r| r.value),
            unit: rule.and_then(|r| r.unit.clone()),
            hold_s: rule.and_then(|r| r.hold_s),
        }
    }
}

impl From<&c::ChecklistFile> for dto::ChecklistFile {
    fn from(v: &c::ChecklistFile) -> Self {
        Self {
            format: v.format.clone(),
            name: v.name.clone(),
            steps: v.steps.iter().map(Into::into).collect(),
        }
    }
}

/// A step on the wire, with whether a flight began since its tick.
#[must_use]
pub fn step(v: &c::Step, flown_since: bool) -> dto::ChecklistStep {
    let rule = v.rule.as_ref();
    dto::ChecklistStep {
        id: v.id.clone(),
        kind: v.kind.into(),
        title: v.title.clone(),
        severity: v.severity.into(),
        item: rule.and_then(|r| r.item.clone()),
        param: rule.and_then(|r| r.param.clone()),
        op: rule.map(|r| r.op.into()),
        value: rule.map(|r| r.value),
        unit: rule.and_then(|r| r.unit.clone()),
        hold_s: rule.and_then(|r| r.hold_s),
        tick: v.tick.as_ref().map(Into::into),
        flown_since,
    }
}

impl From<&uxduck_core::domain::preflight::ParamOffer> for dto::ParamOffer {
    fn from(v: &uxduck_core::domain::preflight::ParamOffer) -> Self {
        Self {
            step_id: v.step_id.clone(),
            name: v.name.clone(),
            value: v.value,
            current: v.current,
            held: v.held,
            note: v.note.clone(),
        }
    }
}

impl From<&c::RecordStep> for dto::RecordStep {
    fn from(v: &c::RecordStep) -> Self {
        let rule = v.rule.as_ref();
        Self {
            title: v.title.clone(),
            kind: v.kind.into(),
            severity: v.severity.into(),
            status: v.status.into(),
            tick: v.tick.as_ref().map(Into::into),
            item: rule.and_then(|r| r.item.clone()),
            param: rule.and_then(|r| r.param.clone()),
            op: rule.map(|r| r.op.into()),
            target: rule.map(|r| r.value),
            unit: rule.and_then(|r| r.unit.clone()),
            hold_s: rule.and_then(|r| r.hold_s),
            value: v.value,
        }
    }
}

impl From<&c::ChecklistRecord> for dto::ChecklistRecord {
    fn from(v: &c::ChecklistRecord) -> Self {
        Self {
            steps: v.steps.iter().map(Into::into).collect(),
            overridden: v.overridden.iter().map(Into::into).collect(),
            override_by: v.override_by.as_ref().map(Into::into),
            failing: v.failing.iter().map(Into::into).collect(),
        }
    }
}

use uxduck_core::domain::gripper as g;

use crate::dto::gripper as dto;

impl From<g::GripperEnd> for dto::GripperEnd {
    fn from(v: g::GripperEnd) -> Self {
        match v {
            g::GripperEnd::Max => Self::Max,
            g::GripperEnd::Min => Self::Min,
        }
    }
}

impl From<dto::GripperEnd> for g::GripperEnd {
    fn from(v: dto::GripperEnd) -> Self {
        match v {
            dto::GripperEnd::Max => Self::Max,
            dto::GripperEnd::Min => Self::Min,
        }
    }
}

impl From<g::GripperAction> for dto::GripperAction {
    fn from(v: g::GripperAction) -> Self {
        match v {
            g::GripperAction::Open => Self::Open,
            g::GripperAction::Close => Self::Close,
        }
    }
}

impl From<dto::GripperAction> for g::GripperAction {
    fn from(v: dto::GripperAction) -> Self {
        match v {
            dto::GripperAction::Open => Self::Open,
            dto::GripperAction::Close => Self::Close,
        }
    }
}

impl From<g::GripperSetup> for dto::GripperSetup {
    fn from(v: g::GripperSetup) -> Self {
        Self {
            actuator: v.actuator,
            opens_at: v.opens_at.into(),
        }
    }
}

impl From<dto::GripperSetup> for g::GripperSetup {
    fn from(v: dto::GripperSetup) -> Self {
        Self {
            actuator: v.actuator,
            opens_at: v.opens_at.into(),
        }
    }
}

impl From<&g::GripperWiring> for dto::GripperWiring {
    fn from(v: &g::GripperWiring) -> Self {
        Self {
            output: v.output,
            open_button: v.open_button,
            close_button: v.close_button,
            min_us: v.min_us,
            max_us: v.max_us,
            neutral_us: v.neutral_us(),
        }
    }
}

use uxduck_core::domain::controller as c;

use crate::dto::controller as dto;

macro_rules! both_ways {
    ($core:ty, $dto:ty, $($variant:ident),+) => {
        impl From<$core> for $dto {
            fn from(v: $core) -> Self {
                match v { $(<$core>::$variant => Self::$variant),+ }
            }
        }
        impl From<$dto> for $core {
            fn from(v: $dto) -> Self {
                match v { $(<$dto>::$variant => Self::$variant),+ }
            }
        }
    };
}

both_ways!(c::TargetMode, dto::TargetMode, Pinned, FollowsSelection);
both_ways!(c::StickMode, dto::StickMode, Mode1, Mode2, Mode3, Mode4);
both_ways!(
    c::ThrottleRange,
    dto::ThrottleRange,
    FullTravel,
    CentreToTop
);
both_ways!(
    c::GamepadAxis,
    dto::GamepadAxis,
    LeftStickX,
    LeftStickY,
    RightStickX,
    RightStickY,
    LeftZ,
    RightZ
);
both_ways!(
    c::GamepadButton,
    dto::GamepadButton,
    South,
    East,
    North,
    West,
    LeftTrigger,
    RightTrigger,
    LeftTrigger2,
    RightTrigger2,
    Select,
    Start,
    Mode,
    LeftThumb,
    RightThumb,
    DpadUp,
    DpadDown,
    DpadLeft,
    DpadRight
);

impl From<c::AxisCalibration> for dto::AxisCalibration {
    fn from(v: c::AxisCalibration) -> Self {
        Self {
            min: v.min,
            centre: v.centre,
            max: v.max,
        }
    }
}

impl From<dto::AxisCalibration> for c::AxisCalibration {
    fn from(v: dto::AxisCalibration) -> Self {
        Self {
            min: v.min,
            centre: v.centre,
            max: v.max,
        }
    }
}

impl From<c::AxisBinding> for dto::AxisBinding {
    fn from(v: c::AxisBinding) -> Self {
        Self {
            axis: v.axis.into(),
            code: v.code,
            inverted: v.inverted,
            calibration: v.calibration.map(Into::into),
        }
    }
}

impl From<dto::AxisBinding> for c::AxisBinding {
    fn from(v: dto::AxisBinding) -> Self {
        Self {
            axis: v.axis.into(),
            code: v.code,
            inverted: v.inverted,
            calibration: v.calibration.map(Into::into),
        }
    }
}

impl From<&c::ButtonBinding> for dto::ButtonBinding {
    fn from(v: &c::ButtonBinding) -> Self {
        Self {
            button: v.button.into(),
            code: v.code,
            command: (&v.command).into(),
        }
    }
}

impl From<dto::ButtonBinding> for c::ButtonBinding {
    fn from(v: dto::ButtonBinding) -> Self {
        Self {
            button: v.button.into(),
            code: v.code,
            command: v.command.into(),
        }
    }
}

impl From<&c::InputMapping> for dto::InputMapping {
    fn from(v: &c::InputMapping) -> Self {
        Self {
            id: v.id.clone(),
            name: v.name.clone(),
            roll: v.roll.into(),
            pitch: v.pitch.into(),
            yaw: v.yaw.into(),
            throttle: v.throttle.into(),
            throttle_range: v.throttle_range.into(),
            stick_mode: v.stick_mode.map(Into::into),
            deadzone: v.deadzone,
            buttons: v.buttons.iter().map(Into::into).collect(),
            positions: v.positions.iter().map(Into::into).collect(),
            rc_channels: v.rc_channels.iter().map(Into::into).collect(),
            version: v.version,
        }
    }
}

impl From<&c::PositionBinding> for dto::PositionBinding {
    fn from(v: &c::PositionBinding) -> Self {
        Self {
            axis: v.axis.into(),
            code: v.code,
            at: v.at,
            command: (&v.command).into(),
        }
    }
}

impl From<dto::PositionBinding> for c::PositionBinding {
    fn from(v: dto::PositionBinding) -> Self {
        Self {
            axis: v.axis.into(),
            code: v.code,
            at: v.at,
            command: v.command.into(),
        }
    }
}

impl From<&c::Controller> for dto::Controller {
    fn from(v: &c::Controller) -> Self {
        Self {
            id: v.id.clone(),
            name: v.name.clone(),
            connected: v.connected,
            claim: v.claim.as_ref().map(|claim| dto::Claim {
                client_id: claim.client_id.clone(),
                client_name: claim.client_name.clone(),
                operator_id: claim.operator_id.clone(),
            }),
            mode: v.mode.into(),
            target: v.target.clone(),
            pending_target: v.pending_target.clone(),
            mapping_id: v.mapping_id.clone(),
            input_pause: v.input_pause.clone(),
            version: v.version,
        }
    }
}

impl From<&c::RcAxisBinding> for dto::RcAxisBinding {
    fn from(v: &c::RcAxisBinding) -> Self {
        Self {
            channel: v.channel,
            input: v.input.into(),
        }
    }
}
impl From<dto::RcAxisBinding> for c::RcAxisBinding {
    fn from(v: dto::RcAxisBinding) -> Self {
        Self {
            channel: v.channel,
            input: v.input.into(),
        }
    }
}

use uxduck_core::domain::tuning as core;
use uxduck_core::domain::tuning_autotune as autotune;
use uxduck_core::domain::tuning_steps as steps;

use crate::dto::tuning as dto;

impl From<core::Controller> for dto::TuningController {
    fn from(value: core::Controller) -> Self {
        match value {
            core::Controller::Copter => Self::Copter,
            core::Controller::Hover => Self::Hover,
            core::Controller::Plane => Self::Plane,
        }
    }
}

impl From<core::Axis> for dto::TuningAxis {
    fn from(value: core::Axis) -> Self {
        match value {
            core::Axis::Roll => Self::Roll,
            core::Axis::Pitch => Self::Pitch,
            core::Axis::Yaw => Self::Yaw,
        }
    }
}

impl From<core::Term> for dto::TuningTerm {
    fn from(value: core::Term) -> Self {
        match value {
            core::Term::P => Self::P,
            core::Term::I => Self::I,
            core::Term::D => Self::D,
            core::Term::Ff => Self::Ff,
            core::Term::Imax => Self::Imax,
            core::Term::Fltt => Self::Fltt,
            core::Term::Fltd => Self::Fltd,
            core::Term::Flte => Self::Flte,
        }
    }
}

impl From<core::Feel> for dto::TuningFeel {
    fn from(value: core::Feel) -> Self {
        match value {
            core::Feel::Responsiveness => Self::Responsiveness,
            core::Feel::MaxLean => Self::MaxLean,
            core::Feel::MaxRollRate => Self::MaxRollRate,
            core::Feel::MaxPitchRate => Self::MaxPitchRate,
            core::Feel::MaxYawRate => Self::MaxYawRate,
            core::Feel::ClimbSpeed => Self::ClimbSpeed,
            core::Feel::DescentSpeed => Self::DescentSpeed,
            core::Feel::RollLimit => Self::RollLimit,
            core::Feel::PitchUpLimit => Self::PitchUpLimit,
            core::Feel::PitchDownLimit => Self::PitchDownLimit,
            core::Feel::RollResponse => Self::RollResponse,
            core::Feel::PitchResponse => Self::PitchResponse,
        }
    }
}

impl From<core::Unsupported> for dto::TuningUnsupported {
    fn from(value: core::Unsupported) -> Self {
        match value {
            core::Unsupported::Autopilot => Self::Autopilot,
            core::Unsupported::Helicopter => Self::Helicopter,
            core::Unsupported::VehicleType => Self::VehicleType,
        }
    }
}

/// A controller's parts, each parameter given by `param`.
pub fn parts(
    resolved: &core::Resolved,
    param: impl Fn(&str) -> dto::TuningParam,
) -> dto::TuningControllerParts {
    dto::TuningControllerParts {
        controller: resolved.controller.into(),
        feel: resolved
            .feel
            .iter()
            .map(|f| dto::TuningFeelPart {
                feel: f.role.into(),
                param: param(&f.name),
            })
            .collect(),
        rates: resolved
            .rates
            .iter()
            .map(|r| dto::TuningRate {
                axis: r.axis.into(),
                terms: r
                    .terms
                    .iter()
                    .map(|t| dto::TuningTermPart {
                        term: t.role.into(),
                        param: param(&t.name),
                    })
                    .collect(),
            })
            .collect(),
        angles: resolved
            .angles
            .iter()
            .map(|a| dto::TuningAngle {
                axis: a.role.into(),
                param: param(&a.name),
            })
            .collect(),
        steps: Vec::new(),
        autotune: None,
    }
}

impl From<dto::TuningController> for core::Controller {
    fn from(value: dto::TuningController) -> Self {
        match value {
            dto::TuningController::Copter => Self::Copter,
            dto::TuningController::Hover => Self::Hover,
            dto::TuningController::Plane => Self::Plane,
        }
    }
}

impl From<steps::Step> for dto::TuningStep {
    fn from(value: steps::Step) -> Self {
        match value {
            steps::Step::Prepare => Self::Prepare,
            steps::Step::FirstFlight => Self::FirstFlight,
            steps::Step::Filters => Self::Filters,
            steps::Step::Tune => Self::Tune,
            steps::Step::Feel => Self::Feel,
        }
    }
}

impl From<dto::TuningChemistry> for steps::Chemistry {
    fn from(value: dto::TuningChemistry) -> Self {
        match value {
            dto::TuningChemistry::Lipo => Self::Lipo,
            dto::TuningChemistry::LipoHv => Self::LipoHv,
            dto::TuningChemistry::LiIon => Self::LiIon,
        }
    }
}

impl From<autotune::Phase> for dto::TuningAutotunePhase {
    fn from(v: autotune::Phase) -> Self {
        match v {
            autotune::Phase::Running => Self::Running,
            autotune::Phase::Done => Self::Done,
            autotune::Phase::Kept => Self::Kept,
            autotune::Phase::Stopped => Self::Stopped,
            autotune::Phase::Failed => Self::Failed,
        }
    }
}

impl dto::TuningAutotune {
    /// A driver and where the transmitter enters it.
    #[must_use]
    pub fn of(driver: &autotune::Driver, entry: autotune::Entry) -> Self {
        Self {
            mode: driver.mode.to_owned(),
            mode_number: driver.mode_number,
            axes: driver.axes.to_owned(),
            strength: driver.strength.to_owned(),
            switch_option: driver.switch_option,
            keeps: driver.keeps.to_owned(),
            channel: entry.channel,
            positions: entry.positions,
            switches: entry.switches,
        }
    }
}

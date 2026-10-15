use uxduck_core::domain::practice as core;

use crate::dto::practice as dto;

impl From<core::AttemptState> for dto::PracticeState {
    fn from(v: core::AttemptState) -> Self {
        match v {
            core::AttemptState::Starting => Self::Starting,
            core::AttemptState::Running => Self::Running,
            core::AttemptState::Passed => Self::Passed,
            core::AttemptState::Failed => Self::Failed,
            core::AttemptState::GivenUp => Self::GivenUp,
        }
    }
}

impl From<core::Audience> for dto::PracticeAudience {
    fn from(v: core::Audience) -> Self {
        match v {
            core::Audience::FirstFlight => Self::FirstFlight,
            core::Audience::Basics => Self::Basics,
        }
    }
}

impl From<&core::Attempt> for dto::PracticeAttempt {
    fn from(v: &core::Attempt) -> Self {
        Self {
            id: v.id.clone(),
            exercise_id: v.exercise_id.clone(),
            guide: v.guide,
            vehicle_id: v.vehicle_id.clone(),
            state: v.state.into(),
            step: v.step,
            line: v.line.clone(),
            measure: v.measure,
            features: v
                .features
                .iter()
                .map(|f| dto::PracticeFeature {
                    id: f.id.clone(),
                    label: f.label.clone(),
                    latitude_deg: f.latitude_deg,
                    longitude_deg: f.longitude_deg,
                    radius_m: f.radius_m,
                    outline: f
                        .outline
                        .iter()
                        .map(|&(latitude_deg, longitude_deg)| dto::PracticePoint {
                            latitude_deg,
                            longitude_deg,
                        })
                        .collect(),
                })
                .collect(),
            answered: v.answered.iter().cloned().collect(),
            started_at_us: v.started_at_us,
            step_started_at_us: v.step_started_at_us,
            ended_at_us: v.ended_at_us,
            version: v.version,
        }
    }
}

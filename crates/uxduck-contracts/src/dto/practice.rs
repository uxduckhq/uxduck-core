//! Practice (ADR-0024, S100): the exercises as the tab lists them, an
//! attempt as it runs, and the `practice/*` calls.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PracticeAudience {
    /// "For a first flight".
    FirstFlight,
    /// "Once you know the basics".
    Basics,
}

/// A question of "Read the panel", answered with `practice/answer`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PracticeQuestion {
    pub id: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/// One step: what to do, and a hint shown once it has taken a while.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PracticeStep {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Seconds into the step before the hint shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint_after_s: Option<f64>,
}

/// The operator's best attempt at an exercise (X6).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PracticeBest {
    pub passed: bool,
    pub line: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measure: Option<f64>,
    /// `m`, `%` or `s`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/// One exercise as the tab lists it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PracticeExercise {
    pub id: String,
    pub title: String,
    pub audience: PracticeAudience,
    /// The simulated vehicles it fits (S117); the tab lists only those
    /// fitting the vehicle it follows.
    pub vehicles: Vec<crate::dto::link::SimVehicle>,
    /// A "Get started" guide (S117), started with `practice/guide` on the
    /// vehicle as it is; never listed with the exercises in the tab.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub guide: bool,
    pub goal: String,
    /// What to do, step by step.
    pub steps: Vec<PracticeStep>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<PracticeQuestion>,
    /// Why the vehicle asked about cannot run it, such as not being simulated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub best: Option<PracticeBest>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PracticeState {
    /// The vehicle is being started and set up; nothing is judged yet.
    Starting,
    Running,
    Passed,
    Failed,
    GivenUp,
}

/// A ring or circle of an attempt, or an area or a line with an outline,
/// drawn on every map of its vehicle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PracticeFeature {
    pub id: String,
    pub label: String,
    /// The circle's centre, or where the label goes.
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    /// Zero for an area or a line.
    pub radius_m: f64,
    /// An area's corners (three or more) or a line's two ends.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outline: Vec<PracticePoint>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PracticePoint {
    pub latitude_deg: f64,
    pub longitude_deg: f64,
}

/// An attempt: the latest on each vehicle, pushed as the `practice-attempt`
/// entity, by vehicle id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PracticeAttempt {
    pub id: String,
    pub exercise_id: String,
    /// An attempt at a guide (S117), not at an exercise of the tab.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub guide: bool,
    pub vehicle_id: String,
    pub state: PracticeState,
    /// The current step, from 0.
    pub step: u32,
    /// Why it ended, in one line, such as "Passed. Home with 31% left."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measure: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<PracticeFeature>,
    /// Questions answered right so far.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub answered: Vec<String>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub started_at_us: u64,
    /// When the current step began, for its hint's time.
    #[schemars(schema_with = "crate::safe::integer")]
    pub step_started_at_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub ended_at_us: Option<u64>,
    #[schemars(schema_with = "crate::safe::integer")]
    pub version: u64,
}

/// `practice/list`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PracticeListParams {
    /// The vehicle to say about: which exercises it can run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PracticeListResult {
    pub exercises: Vec<PracticeExercise>,
}

/// `practice/start`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PracticeStartParams {
    pub exercise_id: String,
    pub vehicle_id: String,
}

/// `practice/answer`: an answer to one of "Read the panel"'s questions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PracticeAnswerParams {
    pub vehicle_id: String,
    pub question: String,
    pub answer: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PracticeAnswerResult {
    pub right: bool,
    pub attempt: PracticeAttempt,
}

/// `practice/stop`, `practice/status` and `practice/guide`: the vehicle's attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PracticeVehicleParams {
    pub vehicle_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PracticeStatusResult {
    /// Its latest attempt, running or ended; none if it had none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<PracticeAttempt>,
}

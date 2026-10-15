//! Practice (ADR-0024, S100): short exercises on a simulated vehicle, each
//! a declarative definition judged by the engine, and the attempts at them,
//! kept in the journal on the operator. An exercise is data (X2): a start,
//! a goal and steps in words, and named checks over the vehicle's state.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::domain::link::SimVehicle;

/// Who an exercise is for: the tab lists them under two headings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Audience {
    /// "For a first flight".
    FirstFlight,
    /// "Once you know the basics".
    Basics,
}

/// A place relative to home: a bearing clockwise from north and a distance.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Offset {
    pub bearing_deg: f64,
    pub distance_m: f64,
}

/// How an attempt begins (X4): the vehicle restarted in the air at a
/// height over home, then, as `practice-start`, moved and set up.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExerciseStart {
    /// Height above home it starts holding at; none on the ground.
    #[serde(default)]
    pub height_m: f64,
    /// Starts on the ground, disarmed, rather than holding in the air.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub on_ground: bool,
    /// Where it is moved before the attempt is judged; home when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<Offset>,
    /// The battery's remaining charge set once it is there, in percent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery_pct: Option<f64>,
    /// Parameters set once the take-off is done, such as a larger battery so
    /// a low charge falls slowly enough to fly home on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<StartParameter>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartParameter {
    pub name: String,
    pub value: f64,
}

/// A shape on the map, relative to home: a circle (a ring to reach, a
/// circle to land in), or with an outline, an area (a box, a field) of
/// three points or more, or a line of two (a gate).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExerciseFeature {
    pub id: String,
    pub label: String,
    /// The circle's centre, or where the label goes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<Offset>,
    #[serde(default)]
    pub radius_m: f64,
    /// Its corners or ends, from home.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outline: Vec<Offset>,
}

/// What a question asks the person to read off Telemetry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reading {
    HeightAboveHome,
    Mode,
    BatteryPct,
}

/// A question for "Read the panel", answered with `practice/answer`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Question {
    pub id: String,
    pub text: String,
    pub reads: Reading,
    /// How far off a number may be; a mode must match exactly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tolerance: Option<f64>,
    /// The unit the answer is in, for the form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/// The judge's vocabulary (ADR-0024 "The judge's vocabulary"): each check
/// is a pure function of the vehicle's state at a moment (X5).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "check",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Check {
    /// A command of a kind sent by an operator's client since the step began:
    /// `setMode` (with `mode`), `rtl` (or the mode RTL or QRTL chosen),
    /// `land` (or LAND, QLAND or SURFACE chosen), `goTo` or `arm`.
    CommandSent {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<String>,
    },
    /// The vehicle reports this mode, or one of `or`: a quadplane's return
    /// is RTL, then QRTL as it comes in to land.
    Mode {
        mode: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        or: Vec<String>,
    },
    /// Inside a feature's circle or area, for at least `for_s` seconds
    /// without leaving it when given, and in `mode` all that time when given.
    Within {
        feature: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        for_s: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<String>,
    },
    /// Outside a feature's circle or area, for at least `for_s` seconds
    /// when given.
    Outside {
        feature: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        for_s: Option<f64>,
    },
    /// A gate's line crossed since the step began, between two points of the
    /// track both within a band of height above home.
    GateCrossed {
        feature: String,
        min_m: f64,
        max_m: f64,
    },
    /// A mission of at least `items` places uploaded by the operator since
    /// the attempt began.
    MissionUploaded { items: u32 },
    /// A place of that mission outside an area by more than `by_m`.
    MissionOutside { feature: String, by_m: f64 },
    /// The track flown in Auto covers at least `fraction` of an area,
    /// counted as [`Coverage`] says.
    MissionCovers { feature: String, fraction: f64 },
    /// Gates of later steps crossed, out of order, this many times since
    /// the attempt began.
    OutOfOrder { count: u32 },
    /// A controller connected, claimed and driving the vehicle.
    ControllerClaimed,
    /// None is (the controller released or unplugged).
    NoController,
    /// Height above home in a band.
    HeightBetween { min_m: f64, max_m: f64 },
    /// On the ground and disarmed.
    Landed,
    /// Armed and off the ground.
    Airborne,
    /// Armed, anywhere: a rover, a boat or a submarine arming to go.
    Armed,
    /// The battery's remaining charge under a percentage.
    BatteryBelow { pct: f64 },
    /// A question answered right (`practice/answer`).
    Answered { question: String },
    /// One question answered wrong this many times.
    WrongAnswers { count: u32 },
    /// Time since the step began, or since the attempt did.
    Elapsed {
        s: f64,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        since_start: bool,
    },
    /// A parameter set by the operator, since the attempt began, to at
    /// least a value.
    ParameterAtLeast { name: String, value: f64 },
}

/// One step: passed once all its checks hold, then the next begins; the last
/// one passes the attempt. Its hint shows after `hint_after_s`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint_after_s: Option<f64>,
    pub all: Vec<Check>,
}

/// A way to fail, watched at every step: all its checks holding ends the
/// attempt with `says`, in which `{distance:<feature>}`, `{battery}` and
/// `{elapsed}` are filled in.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    pub all: Vec<Check>,
    pub says: String,
}

/// The one value kept per attempt (X6), and whether more is better.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "measure",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Measure {
    DistanceTo {
        feature: String,
    },
    BatteryRemaining,
    Elapsed,
    /// The longest time inside a feature without leaving it.
    LongestStay {
        feature: String,
    },
    /// How much of an area the track flown in Auto covers, in percent.
    Coverage {
        feature: String,
    },
}

/// How coverage is counted (ADR-0024 "Plan the survey"): the area's points
/// on a grid this far apart, each covered once a point of the track flown
/// in Auto came within reach of it.
#[derive(Debug)]
pub struct Coverage;

impl Coverage {
    pub const GRID_M: f64 = 10.0;
    pub const REACH_M: f64 = 15.0;
    pub const MODE: &str = "AUTO";
}

impl Measure {
    /// A larger value is a better result.
    #[must_use]
    pub const fn higher_is_better(&self) -> bool {
        matches!(
            self,
            Self::BatteryRemaining | Self::LongestStay { .. } | Self::Coverage { .. }
        )
    }

    /// Its unit, for the tab: `m`, `%` or `s`.
    #[must_use]
    pub const fn unit(&self) -> &'static str {
        match self {
            Self::DistanceTo { .. } => "m",
            Self::BatteryRemaining | Self::Coverage { .. } => "%",
            Self::Elapsed | Self::LongestStay { .. } => "s",
        }
    }
}

/// One exercise (X2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Exercise {
    /// Such as `bring-it-home`.
    pub id: String,
    /// Bumped when the exercise changes, so results say which they were.
    pub version: u32,
    pub title: String,
    pub audience: Audience,
    /// The simulated vehicles it fits (S117): its modes and checks are
    /// theirs. Every shipped exercise names at least one.
    #[serde(default)]
    pub vehicles: Vec<SimVehicle>,
    /// A guide (S117, "Get started"): started on the vehicle as it is, with
    /// no restart and no set-up, never failed, and one per simulated vehicle.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub guide: bool,
    pub goal: String,
    pub start: ExerciseStart,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<ExerciseFeature>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<Question>,
    pub steps: Vec<Step>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fails: Vec<Rule>,
    /// The line when it is passed, filled in as a rule's `says` is.
    pub passed: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measure: Option<Measure>,
    /// Needs a controller claimed for the vehicle (ADR-0024's controller rule).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub needs_controller: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AttemptState {
    /// The vehicle is being restarted and set up; nothing is judged yet.
    Starting,
    Running,
    Passed,
    Failed,
    GivenUp,
}

impl AttemptState {
    #[must_use]
    pub const fn is_over(self) -> bool {
        matches!(self, Self::Passed | Self::Failed | Self::GivenUp)
    }
}

/// A feature of an attempt, placed: drawn on every map of the vehicle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlacedFeature {
    pub id: String,
    pub label: String,
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    pub radius_m: f64,
    /// An area's corners or a line's ends, as latitude and longitude.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outline: Vec<(f64, f64)>,
}

/// One run of an exercise on one simulated vehicle (ADR-0024 "Attempt").
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attempt {
    pub id: String,
    pub exercise_id: String,
    pub exercise_version: u32,
    /// An attempt at a guide (S117): ended by starting any other exercise.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub guide: bool,
    pub vehicle_id: String,
    pub link_id: String,
    /// Whose attempt it is: results are the operator's (X7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_id: Option<String>,
    pub state: AttemptState,
    /// The current step, from 0; the number of steps once passed.
    pub step: u32,
    /// Why it ended, in one line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measure: Option<f64>,
    /// Where home is: the simulation's start.
    pub home_latitude_deg: f64,
    pub home_longitude_deg: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<PlacedFeature>,
    /// Questions answered right, and wrong answers by question.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub answered: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub wrong: BTreeMap<String, u32>,
    /// Set up as `practice-start`: moved to the start and its battery set.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub set_up: bool,
    /// The parameters its start changed, with the values they had before,
    /// written back when the attempt ends (maintainer, 2026-10-13: an
    /// exercise leaves the simulated vehicle as it found it).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restore: Vec<StartParameter>,
    pub started_at_us: u64,
    pub step_started_at_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_us: Option<u64>,
    pub version: u64,
}

/// The best attempt an operator made at an exercise (X6): a pass beats a
/// failure, then the better measure.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Best {
    pub passed: bool,
    pub line: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measure: Option<f64>,
}

impl Best {
    /// Whether `other` is better, for an exercise whose measure is better
    /// higher when `higher` is set.
    #[must_use]
    pub fn beaten_by(&self, other: &Self, higher: bool) -> bool {
        if self.passed != other.passed {
            return other.passed;
        }
        match (self.measure, other.measure) {
            (Some(was), Some(now)) => {
                if higher {
                    now > was
                } else {
                    now < was
                }
            }
            (None, Some(_)) => true,
            _ => false,
        }
    }
}

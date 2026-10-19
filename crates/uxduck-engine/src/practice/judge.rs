//! The judge (ADR-0024 X3, X5): an attempt's checks over the vehicle's
//! state at a moment, as pure functions, so the same state always gives
//! the same verdict and an attempt can be replayed.

use uxduck_core::domain::command::Command;
use uxduck_core::domain::practice::{
    Attempt, Check, Coverage, Exercise, Measure, Offset, PlacedFeature, Question, Reading,
};

/// Earth's mean radius, for distances of a few hundred metres.
const EARTH_M: f64 = 6_371_000.0;
/// Above this height over home, or while armed, it is not landed.
const LANDED_BELOW_M: f64 = 3.0;

/// What the judge reads of the vehicle at a moment.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct View {
    pub now_us: u64,
    pub position: Option<(f64, f64)>,
    pub height_m: Option<f64>,
    pub mode: Option<String>,
    pub armed: bool,
    /// What the autopilot says of its landed state, when it says.
    pub on_ground: Option<bool>,
    pub battery_pct: Option<f64>,
    /// Commands an operator's client sent this vehicle, with when (X5): not
    /// the agent's, not the attempt's own set-up.
    pub commands: Vec<(u64, Command)>,
    /// Parameters an operator set since the attempt began, with when: the
    /// name and the value set.
    pub parameters_set: Vec<(u64, String, f64)>,
    /// Where it has been since the attempt began, oldest first, ending now.
    pub track: Vec<Sample>,
    /// A controller is connected, claimed and driving it.
    pub controller: bool,
    /// The places of a mission the operator uploaded since the attempt
    /// began, in order.
    pub mission: Option<Vec<(f64, f64)>>,
    /// That mission's commands, in order (S132).
    pub mission_commands: Option<Vec<u16>>,
    /// The `MAV_FRAME` of each of its waypoints.
    pub mission_frames: Option<Vec<u8>>,
    /// The operator uploaded a fence since the attempt began.
    pub fence_uploaded: bool,
    /// Rally points the operator uploaded since the attempt began.
    pub rally: Vec<(f64, f64)>,
    /// Whether the vehicle says it is outside its fence, when it says.
    pub fence_breached: Option<bool>,
    /// Its height over the ground now, by the autopilot's terrain.
    pub clearance_m: Option<f64>,
}

/// One point of an attempt's track.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    pub at_us: u64,
    pub position: (f64, f64),
    pub height_m: Option<f64>,
    pub mode: Option<String>,
    /// Over the ground, by the autopilot's terrain, when it knows.
    pub clearance_m: Option<f64>,
}

/// Metres east and north of `origin`, near enough flat for a field.
fn local(origin: (f64, f64), at: (f64, f64)) -> (f64, f64) {
    let east = (at.1 - origin.1).to_radians() * EARTH_M * origin.0.to_radians().cos();
    let north = (at.0 - origin.0).to_radians() * EARTH_M;
    (east, north)
}

/// An area's corners in flat metres from its first.
fn corners(f: &PlacedFeature) -> Vec<(f64, f64)> {
    f.outline.iter().map(|&c| local(f.outline[0], c)).collect()
}

/// Whether a point is inside a feature: its circle, or its area.
fn contains(f: &PlacedFeature, at: (f64, f64)) -> bool {
    if f.outline.len() < 3 {
        return distance_m(at, (f.latitude_deg, f.longitude_deg)) <= f.radius_m;
    }
    inside(&corners(f), local(f.outline[0], at))
}

/// Whether a flat point is inside flat corners.
fn inside(corners: &[(f64, f64)], p: (f64, f64)) -> bool {
    let mut inside = false;
    let mut j = corners.len() - 1;
    for (i, &a) in corners.iter().enumerate() {
        let b = corners[j];
        if (a.1 > p.1) != (b.1 > p.1) && p.0 < (b.0 - a.0) * (p.1 - a.1) / (b.1 - a.1) + a.0 {
            inside = !inside;
        }
        j = i;
    }
    inside
}

fn feature<'a>(attempt: &'a Attempt, id: &str) -> Option<&'a PlacedFeature> {
    attempt.features.iter().find(|f| f.id == id)
}

/// How long the track has kept to `keep` without a break, up to now; none
/// when it does not now.
fn kept_s(view: &View, keep: impl Fn(&Sample) -> bool) -> Option<f64> {
    let now = Sample {
        at_us: view.now_us,
        position: view.position?,
        height_m: view.height_m,
        mode: view.mode.clone(),
        clearance_m: view.clearance_m,
    };
    if !keep(&now) {
        return None;
    }
    let since = view
        .track
        .iter()
        .rev()
        .take_while(|s| keep(s))
        .last()
        .map_or(view.now_us, |s| s.at_us);
    Some(seconds(view.now_us.saturating_sub(since)))
}

fn in_mode(sample: &Sample, mode: Option<&str>) -> bool {
    mode.is_none_or(|m| {
        sample
            .mode
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case(m))
    })
}

/// The longest the track stayed inside a feature without leaving.
fn longest_stay_s(attempt: &Attempt, view: &View, id: &str) -> Option<f64> {
    let f = feature(attempt, id)?;
    let (mut longest, mut since) = (0_u64, None::<u64>);
    for s in &view.track {
        if contains(f, s.position) {
            let from = *since.get_or_insert(s.at_us);
            longest = longest.max(s.at_us.saturating_sub(from));
        } else {
            since = None;
        }
    }
    Some(seconds(longest))
}

/// How far a flat point is from a segment.
fn from_segment(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let length = dx.mul_add(dx, dy * dy);
    let t = if length > 0.0 {
        ((p.0 - a.0).mul_add(dx, (p.1 - a.1) * dy) / length).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (p.0 - t.mul_add(dx, a.0)).hypot(p.1 - t.mul_add(dy, a.1))
}

/// How far outside an area a point is; zero inside.
fn outside_by(f: &PlacedFeature, at: (f64, f64)) -> f64 {
    let corners = corners(f);
    let p = local(f.outline[0], at);
    if corners.len() < 3 || inside(&corners, p) {
        return 0.0;
    }
    (0..corners.len())
        .map(|i| from_segment(p, corners[i], corners[(i + 1) % corners.len()]))
        .fold(f64::INFINITY, f64::min)
}

/// The share of an area the track flown in Auto has covered, 0 to 1.
fn coverage(attempt: &Attempt, view: &View, id: &str) -> Option<f64> {
    let f = feature(attempt, id).filter(|f| f.outline.len() >= 3)?;
    let corners = corners(f);
    let flown: Vec<(f64, f64)> = view
        .track
        .iter()
        .filter(|s| in_mode(s, Some(Coverage::MODE)))
        .map(|s| local(f.outline[0], s.position))
        .collect();
    let (mut lo, mut hi) = (
        (f64::INFINITY, f64::INFINITY),
        (f64::NEG_INFINITY, f64::NEG_INFINITY),
    );
    for c in &corners {
        lo = (lo.0.min(c.0), lo.1.min(c.1));
        hi = (hi.0.max(c.0), hi.1.max(c.1));
    }
    let reach = Coverage::REACH_M * Coverage::REACH_M;
    let (mut cells, mut covered) = (0_u32, 0_u32);
    let mut y = lo.1 + Coverage::GRID_M / 2.0;
    while y < hi.1 {
        let mut x = lo.0 + Coverage::GRID_M / 2.0;
        while x < hi.0 {
            if inside(&corners, (x, y)) {
                cells += 1;
                if flown
                    .iter()
                    .any(|p| (p.0 - x).mul_add(p.0 - x, (p.1 - y) * (p.1 - y)) <= reach)
                {
                    covered += 1;
                }
            }
            x += Coverage::GRID_M;
        }
        y += Coverage::GRID_M;
    }
    (cells > 0).then(|| f64::from(covered) / f64::from(cells))
}

/// Whether segments `a`–`b` and `c`–`d` cross, in flat metres.
fn crosses(a: (f64, f64), b: (f64, f64), c: (f64, f64), d: (f64, f64)) -> bool {
    let side = |p: (f64, f64), q: (f64, f64), r: (f64, f64)| {
        (q.0 - p.0) * (r.1 - p.1) - (q.1 - p.1) * (r.0 - p.0)
    };
    let (d1, d2) = (side(c, d, a), side(c, d, b));
    let (d3, d4) = (side(a, b, c), side(a, b, d));
    d1 * d2 < 0.0 && d3 * d4 < 0.0
}

/// How many times the track crossed a gate's line since `from_us`, at any height.
fn crossings(attempt: &Attempt, view: &View, id: &str, from_us: u64) -> usize {
    let Some(gate) = feature(attempt, id).filter(|f| f.outline.len() == 2) else {
        return 0;
    };
    let origin = gate.outline[0];
    let (c, d) = (
        local(origin, gate.outline[0]),
        local(origin, gate.outline[1]),
    );
    let track: Vec<&Sample> = view.track.iter().filter(|s| s.at_us >= from_us).collect();
    track
        .windows(2)
        .filter(|pair| {
            crosses(
                local(origin, pair[0].position),
                local(origin, pair[1].position),
                c,
                d,
            )
        })
        .count()
}

/// Gates of steps after this one crossed since the attempt began.
fn out_of_order(exercise: &Exercise, attempt: &Attempt, view: &View) -> usize {
    exercise
        .steps
        .iter()
        .skip(attempt.step as usize + 1)
        .flat_map(|step| &step.all)
        .filter_map(|check| match check {
            Check::GateCrossed { feature, .. } => Some(feature),
            _ => None,
        })
        .map(|gate| crossings(attempt, view, gate, attempt.started_at_us))
        .sum()
}

/// The gate's line crossed between two points of the track since the step
/// began, both in the height band.
fn gate_crossed(attempt: &Attempt, view: &View, id: &str, band: (f64, f64)) -> bool {
    let Some(gate) = feature(attempt, id).filter(|f| f.outline.len() == 2) else {
        return false;
    };
    let origin = gate.outline[0];
    let (c, d) = (origin, gate.outline[1]);
    let (c, d) = (local(origin, c), local(origin, d));
    let in_band = |s: &Sample| s.height_m.is_some_and(|h| (band.0..=band.1).contains(&h));
    let now = view.position.map(|position| Sample {
        at_us: view.now_us,
        position,
        height_m: view.height_m,
        mode: view.mode.clone(),
        clearance_m: view.clearance_m,
    });
    let track: Vec<&Sample> = view
        .track
        .iter()
        .chain(now.as_ref())
        .filter(|s| s.at_us >= attempt.step_started_at_us)
        .collect();
    track.windows(2).any(|pair| {
        in_band(pair[0])
            && in_band(pair[1])
            && crosses(
                local(origin, pair[0].position),
                local(origin, pair[1].position),
                c,
                d,
            )
    })
}

#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    Keep,
    /// The step passed; this one is next.
    Step(u32),
    Passed {
        line: String,
        measure: Option<f64>,
    },
    Failed {
        line: String,
        measure: Option<f64>,
    },
}

/// The place `offset` from a point.
#[must_use]
pub fn place(from: (f64, f64), offset: Offset) -> (f64, f64) {
    let (lat, lon) = (from.0.to_radians(), from.1.to_radians());
    let bearing = offset.bearing_deg.to_radians();
    let angle = offset.distance_m / EARTH_M;
    let lat2 = (lat.sin() * angle.cos() + lat.cos() * angle.sin() * bearing.cos()).asin();
    let lon2 =
        lon + (bearing.sin() * angle.sin() * lat.cos()).atan2(angle.cos() - lat.sin() * lat2.sin());
    (lat2.to_degrees(), lon2.to_degrees())
}

/// Metres between two points.
#[must_use]
pub fn distance_m(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (lat1, lat2) = (a.0.to_radians(), b.0.to_radians());
    let dlat = lat2 - lat1;
    let dlon = (b.1 - a.1).to_radians();
    let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * EARTH_M * h.sqrt().asin()
}

/// The exercise's features placed around home.
#[must_use]
pub fn place_features(exercise: &Exercise, home: (f64, f64)) -> Vec<PlacedFeature> {
    exercise
        .features
        .iter()
        .map(|f| {
            let (latitude_deg, longitude_deg) = f.offset.map_or(home, |o| place(home, o));
            PlacedFeature {
                id: f.id.clone(),
                label: f.label.clone(),
                latitude_deg,
                longitude_deg,
                radius_m: f.radius_m,
                outline: f.outline.iter().map(|&o| place(home, o)).collect(),
            }
        })
        .collect()
}

fn distance_to(attempt: &Attempt, view: &View, id: &str) -> Option<f64> {
    let f = feature(attempt, id)?;
    Some(distance_m(
        view.position?,
        (f.latitude_deg, f.longitude_deg),
    ))
}

/// Landed and disarmed: what "on the ground" means to an exercise.
#[must_use]
pub fn landed(view: &View) -> bool {
    !view.armed && view.on_ground != Some(false) && view.height_m.is_none_or(|h| h < LANDED_BELOW_M)
}

fn airborne(view: &View) -> bool {
    view.armed && view.on_ground != Some(true) && view.height_m.is_none_or(|h| h >= 1.0)
}

/// A command of a kind, by the names exercises use for them.
fn is_kind(command: &Command, kind: &str, mode: Option<&str>) -> bool {
    let set_mode = |wanted: &str| matches!(command, Command::SetMode { mode } if mode.eq_ignore_ascii_case(wanted));
    match kind {
        "setMode" => mode.is_some_and(set_mode),
        // A quadplane's own return and landing count, as Control's Land is
        // QLAND on one and SURFACE on a submarine.
        "rtl" => matches!(command, Command::Rtl) || set_mode("RTL") || set_mode("QRTL"),
        "land" => {
            matches!(command, Command::Land)
                || set_mode("LAND")
                || set_mode("QLAND")
                || set_mode("SURFACE")
        }
        "goTo" => matches!(command, Command::GoTo { .. }),
        "arm" => matches!(command, Command::Arm),
        _ => false,
    }
}

fn holds(check: &Check, exercise: &Exercise, attempt: &Attempt, view: &View) -> bool {
    match check {
        Check::CommandSent { command, mode } => view.commands.iter().any(|(at, sent)| {
            *at >= attempt.step_started_at_us && is_kind(sent, command, mode.as_deref())
        }),
        Check::Mode { mode, or } => view.mode.as_deref().is_some_and(|m| {
            m.eq_ignore_ascii_case(mode) || or.iter().any(|o| m.eq_ignore_ascii_case(o))
        }),
        Check::Within {
            feature: id,
            for_s,
            mode,
        } => feature(attempt, id).is_some_and(|f| {
            kept_s(view, |s| {
                contains(f, s.position) && in_mode(s, mode.as_deref())
            })
            .is_some_and(|kept| kept >= for_s.unwrap_or(0.0))
        }),
        Check::Outside { feature: id, for_s } => feature(attempt, id).is_some_and(|f| {
            kept_s(view, |s| !contains(f, s.position))
                .is_some_and(|kept| kept >= for_s.unwrap_or(0.0))
        }),
        Check::GateCrossed {
            feature: id,
            min_m,
            max_m,
        } => gate_crossed(attempt, view, id, (*min_m, *max_m)),
        Check::MissionUploaded { items } => view
            .mission
            .as_ref()
            .is_some_and(|m| m.len() >= *items as usize),
        Check::MissionOutside { feature: id, by_m } => feature(attempt, id)
            .zip(view.mission.as_ref())
            .is_some_and(|(f, m)| m.iter().any(|&at| outside_by(f, at) > *by_m)),
        Check::MissionHas { command } => view
            .mission_commands
            .as_ref()
            .is_some_and(|c| c.contains(command)),
        Check::MissionLacks { command } => !view
            .mission_commands
            .as_ref()
            .is_some_and(|c| c.contains(command)),
        Check::MissionFrame { frame } => view.mission_frames.as_ref().is_some_and(|frames| {
            // `_INT` frames (5, 6, 11) are their plain twins (0, 3, 10).
            let plain = |f: u8| match f {
                5 => 0,
                6 => 3,
                11 => 10,
                other => other,
            };
            !frames.is_empty() && frames.iter().all(|&f| plain(f) == plain(*frame))
        }),
        Check::FenceUploaded => view.fence_uploaded,
        Check::RallyWithin { feature: id } => {
            feature(attempt, id).is_some_and(|f| view.rally.iter().any(|&r| contains(f, r)))
        }
        Check::FenceBreached => view.fence_breached == Some(true),
        Check::FenceClear => view.fence_breached == Some(false),
        Check::ClearanceBelow { m } => view
            .track
            .iter()
            .filter(|s| s.at_us >= attempt.step_started_at_us)
            .any(|s| s.clearance_m.is_some_and(|c| c < *m)),
        Check::MissionCovers { feature, fraction } => {
            coverage(attempt, view, feature).is_some_and(|c| c >= *fraction)
        }
        Check::OutOfOrder { count } => out_of_order(exercise, attempt, view) >= *count as usize,
        Check::ControllerClaimed => view.controller,
        Check::NoController => !view.controller,
        Check::HeightBetween { min_m, max_m } => view
            .height_m
            .is_some_and(|h| (*min_m..=*max_m).contains(&h)),
        Check::Landed => landed(view),
        Check::Airborne => airborne(view),
        Check::Armed => view.armed,
        Check::BatteryBelow { pct } => view.battery_pct.is_some_and(|b| b < *pct),
        Check::Answered { question } => attempt.answered.contains(question),
        Check::WrongAnswers { count } => attempt.wrong.values().any(|n| n >= count),
        Check::Elapsed { s, since_start } => {
            let from = if *since_start {
                attempt.started_at_us
            } else {
                attempt.step_started_at_us
            };
            seconds(view.now_us.saturating_sub(from)) >= *s
        }
        Check::ParameterAtLeast { name, value } => view
            .parameters_set
            .iter()
            .filter(|(at, set, _)| *at >= attempt.started_at_us && set == name)
            .max_by_key(|(at, ..)| *at)
            .is_some_and(|(.., to)| to >= value),
    }
}

#[allow(clippy::cast_precision_loss, reason = "microseconds of an attempt")]
fn seconds(us: u64) -> f64 {
    us as f64 / 1_000_000.0
}

/// The exercise's measure now.
#[must_use]
pub fn measure(exercise: &Exercise, attempt: &Attempt, view: &View) -> Option<f64> {
    match exercise.measure.as_ref()? {
        Measure::DistanceTo { feature } => distance_to(attempt, view, feature),
        Measure::BatteryRemaining => view.battery_pct,
        Measure::Elapsed => Some(seconds(view.now_us.saturating_sub(attempt.started_at_us))),
        Measure::LongestStay { feature } => longest_stay_s(attempt, view, feature),
        Measure::Coverage { feature } => coverage(attempt, view, feature).map(|c| c * 100.0),
    }
}

/// A line with `{battery}`, `{elapsed}` and `{distance:<feature>}` filled in.
#[must_use]
pub fn fill(line: &str, attempt: &Attempt, view: &View) -> String {
    let mut out = line.replace(
        "{battery}",
        &view
            .battery_pct
            .map_or_else(|| "?".into(), |b| format!("{b:.0}")),
    );
    out = out.replace(
        "{elapsed}",
        &format!(
            "{:.0}",
            seconds(view.now_us.saturating_sub(attempt.started_at_us))
        ),
    );
    while let Some(start) = out.find("{coverage:") {
        let Some(end) = out[start..].find('}') else {
            break;
        };
        let feature = &out[start + "{coverage:".len()..start + end];
        let shown = coverage(attempt, view, feature)
            .map_or_else(|| "?".into(), |c| format!("{:.0}", c * 100.0));
        out.replace_range(start..=start + end, &shown);
    }
    while let Some(start) = out.find("{distance:") {
        let Some(end) = out[start..].find('}') else {
            break;
        };
        let feature = &out[start + "{distance:".len()..start + end];
        let shown =
            distance_to(attempt, view, feature).map_or_else(|| "?".into(), |d| format!("{d:.0}"));
        out.replace_range(start..=start + end, &shown);
    }
    out
}

/// The verdict on a running attempt: a way to fail first, then its step.
#[must_use]
pub fn judge(exercise: &Exercise, attempt: &Attempt, view: &View) -> Verdict {
    for rule in &exercise.fails {
        if !rule.all.is_empty() && rule.all.iter().all(|c| holds(c, exercise, attempt, view)) {
            return Verdict::Failed {
                line: fill(&rule.says, attempt, view),
                measure: measure(exercise, attempt, view),
            };
        }
    }
    let index = attempt.step as usize;
    let Some(step) = exercise.steps.get(index) else {
        return Verdict::Keep;
    };
    if !step.all.iter().all(|c| holds(c, exercise, attempt, view)) {
        return Verdict::Keep;
    }
    if index + 1 < exercise.steps.len() {
        return Verdict::Step(attempt.step + 1);
    }
    Verdict::Passed {
        line: fill(&exercise.passed, attempt, view),
        measure: measure(exercise, attempt, view),
    }
}

/// Whether an answer to a question is right, read from the vehicle now.
#[must_use]
pub fn right(question: &Question, answer: &str, view: &View) -> bool {
    let answer = answer.trim();
    match question.reads {
        Reading::Mode => view.mode.as_deref().is_some_and(|m| {
            m.eq_ignore_ascii_case(answer) || m.replace('_', " ").eq_ignore_ascii_case(answer)
        }),
        Reading::HeightAboveHome | Reading::BatteryPct => {
            let read = if question.reads == Reading::BatteryPct {
                view.battery_pct
            } else {
                view.height_m
            };
            let given = answer.trim_end_matches(['%', 'm', ' ']).parse::<f64>().ok();
            read.zip(given)
                .is_some_and(|(r, g)| (r - g).abs() <= question.tolerance.unwrap_or(0.0))
        }
    }
}

/// Whether a step's hint is due.
#[must_use]
pub fn hint_due(exercise: &Exercise, attempt: &Attempt, now_us: u64) -> Option<String> {
    let step = exercise.steps.get(attempt.step as usize)?;
    let after = step.hint_after_s?;
    (seconds(now_us.saturating_sub(attempt.step_started_at_us)) >= after)
        .then(|| step.hint.clone())
        .flatten()
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use uxduck_core::domain::practice::{AttemptState, Offset};
    use uxduck_core::domain::telemetry::AltitudeFrame;

    use super::*;
    use crate::practice::exercise;

    const HOME: (f64, f64) = (-35.363_261, 149.165_23);
    const S: u64 = 1_000_000;

    fn attempt(id: &str) -> Attempt {
        let exercise = exercise(id).unwrap();
        Attempt {
            id: "pa1".into(),
            exercise_id: id.into(),
            exercise_version: exercise.version,
            guide: exercise.guide,
            vehicle_id: "v1".into(),
            link_id: "sim".into(),
            operator_id: None,
            state: AttemptState::Running,
            step: 0,
            line: None,
            measure: None,
            home_latitude_deg: HOME.0,
            home_longitude_deg: HOME.1,
            features: place_features(exercise, HOME),
            answered: BTreeSet::new(),
            wrong: BTreeMap::new(),
            set_up: true,
            restore: Vec::new(),
            started_at_us: 10 * S,
            step_started_at_us: 10 * S,
            ended_at_us: None,
            version: 1,
        }
    }

    fn flying(at: (f64, f64), height: f64, mode: &str) -> View {
        View {
            now_us: 20 * S,
            position: Some(at),
            height_m: Some(height),
            mode: Some(mode.into()),
            armed: true,
            on_ground: Some(false),
            battery_pct: Some(60.0),
            commands: Vec::new(),
            parameters_set: Vec::new(),
            track: Vec::new(),
            controller: false,
            mission: None,
            ..View::default()
        }
    }

    fn landed_at(at: (f64, f64)) -> View {
        View {
            armed: false,
            on_ground: Some(true),
            height_m: Some(0.2),
            ..flying(at, 0.2, "LAND")
        }
    }

    /// Runs views through the judge as the engine does, one step at a time,
    /// each ending the track of those before it.
    fn run(id: &str, views: &[View]) -> (Attempt, Verdict) {
        let exercise = exercise(id).unwrap();
        let mut attempt = attempt(id);
        let mut last = Verdict::Keep;
        let mut track = Vec::new();
        for view in views {
            if let Some(position) = view.position {
                track.push(Sample {
                    at_us: view.now_us,
                    position,
                    height_m: view.height_m,
                    mode: view.mode.clone(),
                    clearance_m: view.clearance_m,
                });
            }
            let view = &View {
                track: track.clone(),
                ..view.clone()
            };
            last = judge(exercise, &attempt, view);
            if let Verdict::Step(step) = last {
                attempt.step = step;
                attempt.step_started_at_us = view.now_us;
            }
            if matches!(last, Verdict::Passed { .. } | Verdict::Failed { .. }) {
                break;
            }
        }
        (attempt, last)
    }

    #[test]
    fn places_and_distances_agree() {
        let there = place(
            HOME,
            Offset {
                bearing_deg: 90.0,
                distance_m: 400.0,
            },
        );
        assert!((distance_m(HOME, there) - 400.0).abs() < 0.5);
        assert!(there.1 > HOME.1, "east");
    }

    #[test]
    fn mode_tour_passes_on_each_mode_sent_and_reported_in_order() {
        let at = HOME;
        let mut views = Vec::new();
        let mut commands = Vec::new();
        for (n, (command, mode)) in [
            (
                Command::SetMode {
                    mode: "LOITER".into(),
                },
                "LOITER",
            ),
            (
                Command::SetMode {
                    mode: "GUIDED".into(),
                },
                "GUIDED",
            ),
            (Command::Rtl, "RTL"),
            (Command::Land, "LAND"),
        ]
        .into_iter()
        .enumerate()
        {
            let now = (20 + 10 * n as u64) * S;
            commands.push((now, command));
            views.push(View {
                now_us: now + S,
                commands: commands.clone(),
                ..flying(at, 50.0, mode)
            });
        }
        let (_, verdict) = run("mode-tour", &views);
        assert_eq!(
            verdict,
            Verdict::Passed {
                line: "Passed. Four modes, in order.".into(),
                measure: None
            }
        );
        // A mode the vehicle reports but nobody here sent does not count.
        let (attempt, verdict) = run("mode-tour", &[flying(at, 50.0, "LOITER")]);
        assert_eq!((attempt.step, verdict), (0, Verdict::Keep));
    }

    #[test]
    fn bring_it_home_passes_on_return_to_launch_and_landing_home_and_fails_on_a_low_battery() {
        let out = place(
            HOME,
            Offset {
                bearing_deg: 90.0,
                distance_m: 400.0,
            },
        );
        let rtl = vec![(21 * S, Command::Rtl)];
        let mut home = View {
            commands: rtl.clone(),
            battery_pct: Some(28.0),
            ..landed_at(HOME)
        };
        home.now_us = 80 * S;
        let (_, verdict) = run(
            "bring-it-home",
            &[
                View {
                    now_us: 22 * S,
                    commands: rtl,
                    ..flying(out, 50.0, "RTL")
                },
                home,
            ],
        );
        assert_eq!(
            verdict,
            Verdict::Passed {
                line: "Passed. Home with 28% left.".into(),
                measure: Some(28.0)
            }
        );

        let low = View {
            battery_pct: Some(14.0),
            ..flying(out, 50.0, "LOITER")
        };
        let (_, verdict) = run("bring-it-home", &[low]);
        assert!(
            matches!(verdict, Verdict::Failed { ref line, .. } if line == "Failed. The battery ran low with the vehicle still in the air."),
            "{verdict:?}"
        );
    }

    #[test]
    fn land_in_the_circle_measures_the_distance_and_fails_outside() {
        let ring = place(
            HOME,
            Offset {
                bearing_deg: 0.0,
                distance_m: 80.0,
            },
        );
        let near = place(
            ring,
            Offset {
                bearing_deg: 90.0,
                distance_m: 4.0,
            },
        );
        let (_, verdict) = run("land-in-the-circle", &[landed_at(near)]);
        let Verdict::Passed { line, measure } = verdict else {
            panic!("{verdict:?}")
        };
        assert_eq!(line, "Passed. Landed 4 m from the centre.");
        assert!((measure.unwrap() - 4.0).abs() < 0.5);

        let far = place(
            ring,
            Offset {
                bearing_deg: 180.0,
                distance_m: 23.0,
            },
        );
        let (_, verdict) = run("land-in-the-circle", &[landed_at(far)]);
        assert!(
            matches!(verdict, Verdict::Failed { ref line, .. } if line == "Failed. Landed 23 m from the ring."),
            "{verdict:?}"
        );
    }

    #[test]
    fn fly_there_needs_each_ring_in_turn_after_fly_here_and_fails_on_landing() {
        let exercise = exercise("fly-there").unwrap();
        let rings: Vec<(f64, f64)> = exercise
            .features
            .iter()
            .map(|f| place(HOME, f.offset.unwrap()))
            .collect();
        let mut views = Vec::new();
        let mut commands = Vec::new();
        for (n, ring) in rings.iter().enumerate() {
            let now = (30 + 20 * n as u64) * S;
            commands.push((
                now - 10 * S,
                Command::GoTo {
                    latitude_deg: ring.0,
                    longitude_deg: ring.1,
                    altitude_m: 50.0,
                    altitude_frame: AltitudeFrame::AboveHome,
                },
            ));
            views.push(View {
                now_us: now,
                commands: commands.clone(),
                ..flying(*ring, 50.0, "GUIDED")
            });
        }
        let (_, verdict) = run("fly-there", &views);
        assert!(matches!(verdict, Verdict::Passed { .. }), "{verdict:?}");
        // Ring 2 first does nothing.
        let (attempt, verdict) = run("fly-there", &views[1..2]);
        assert_eq!((attempt.step, verdict), (0, Verdict::Keep));
        let (_, verdict) = run("fly-there", &[landed_at(HOME)]);
        assert!(matches!(verdict, Verdict::Failed { .. }));
    }

    #[test]
    fn read_the_panel_checks_answers_against_the_vehicle_now() {
        let exercise = exercise("read-the-panel").unwrap();
        let view = flying(HOME, 61.0, "LOITER");
        let q = |id: &str| exercise.questions.iter().find(|q| q.id == id).unwrap();
        assert!(right(q("height"), "60", &view));
        assert!(right(q("height"), "64 m", &view));
        assert!(!right(q("height"), "70", &view));
        assert!(right(q("mode"), "loiter", &view));
        assert!(!right(q("mode"), "guided", &view));
        assert!(right(q("battery"), "58%", &view));
        let mut attempt = attempt("read-the-panel");
        attempt.wrong.insert("height".into(), 3);
        assert!(matches!(
            judge(exercise, &attempt, &view),
            Verdict::Failed { .. }
        ));
    }

    /// The view at `s` seconds, the attempt having begun at 10 s.
    fn at(s: u64, view: View) -> View {
        View {
            now_us: s * S,
            ..view
        }
    }

    #[test]
    fn timed_delivery_needs_twenty_seconds_at_the_drop_then_home_within_four_minutes() {
        let drop = place(
            HOME,
            Offset {
                bearing_deg: 45.0,
                distance_m: 600.0,
            },
        );
        let near = place(
            drop,
            Offset {
                bearing_deg: 0.0,
                distance_m: 5.0,
            },
        );
        let held = |from: u64, to: u64| -> Vec<View> {
            (from..=to)
                .step_by(5)
                .map(|s| at(s, flying(near, 30.0, "GUIDED")))
                .collect()
        };
        let mut views = held(80, 100);
        views.push(at(160, landed_at(HOME)));
        let (_, verdict) = run("timed-delivery", &views);
        assert_eq!(
            verdict,
            Verdict::Passed {
                line: "Passed. There and back in 150 s.".into(),
                measure: Some(150.0)
            }
        );

        // Fifteen seconds, out, and fifteen more do not make twenty.
        let mut views = held(80, 95);
        views.push(at(100, flying(HOME, 30.0, "GUIDED")));
        views.extend(held(105, 120));
        let (attempt, verdict) = run("timed-delivery", &views);
        assert_eq!((attempt.step, verdict), (0, Verdict::Keep));

        let (_, verdict) = run("timed-delivery", &[at(251, flying(near, 30.0, "GUIDED"))]);
        assert_eq!(
            verdict,
            Verdict::Failed {
                line: "Failed. Four minutes passed.".into(),
                measure: Some(241.0)
            }
        );
    }

    #[test]
    fn parameter_puzzle_needs_the_speed_set_by_the_operator_then_the_ring() {
        let ring = place(
            HOME,
            Offset {
                bearing_deg: 300.0,
                distance_m: 300.0,
            },
        );
        let go = Command::GoTo {
            latitude_deg: ring.0,
            longitude_deg: ring.1,
            altitude_m: 50.0,
            altitude_frame: AltitudeFrame::AboveHome,
        };
        let set = |s: u64, value: f64| (s * S, "WPNAV_SPEED".to_owned(), value);
        let fixed = View {
            parameters_set: vec![set(20, 100.0), set(30, 1000.0)],
            ..at(31, flying(HOME, 50.0, "LOITER"))
        };
        let there = View {
            parameters_set: fixed.parameters_set.clone(),
            commands: vec![(40 * S, go)],
            ..at(70, flying(ring, 50.0, "GUIDED"))
        };
        let (_, verdict) = run("parameter-puzzle", &[fixed, there]);
        assert_eq!(
            verdict,
            Verdict::Passed {
                line: "Passed. At the ring in 60 s.".into(),
                measure: Some(60.0)
            }
        );

        // Set too low, or before the attempt began, it stays slow.
        for parameters_set in [vec![set(20, 400.0)], vec![set(5, 1000.0)]] {
            let view = View {
                parameters_set,
                ..at(31, flying(HOME, 50.0, "LOITER"))
            };
            let (attempt, verdict) = run("parameter-puzzle", &[view]);
            assert_eq!((attempt.step, verdict), (0, Verdict::Keep));
        }
        let (_, verdict) = run("parameter-puzzle", &[at(191, flying(HOME, 50.0, "LOITER"))]);
        assert!(
            matches!(verdict, Verdict::Failed { ref line, .. } if line == "Failed. Three minutes passed.")
        );
    }

    /// `east` and `north` metres from home.
    fn from_home(east: f64, north: f64) -> (f64, f64) {
        let bearing = east.atan2(north).to_degrees();
        place(
            HOME,
            Offset {
                bearing_deg: bearing,
                distance_m: east.hypot(north),
            },
        )
    }

    fn by_hand(view: View) -> View {
        View {
            controller: true,
            ..view
        }
    }

    #[test]
    fn hold_the_box_needs_a_minute_in_each_mode_inside_and_fails_out_of_it_or_without_the_controller()
     {
        let inside = from_home(12.0, -15.0);
        let minute = |from: u64, mode: &str| -> Vec<View> {
            (from..=from + 60)
                .step_by(5)
                .map(|s| by_hand(at(s, flying(inside, 30.0, mode))))
                .collect()
        };
        let mut views = minute(20, "LOITER");
        views.extend(minute(85, "STABILIZE"));
        let (_, verdict) = run("hold-the-box", &views);
        let Verdict::Passed { line, measure } = verdict else {
            panic!("{verdict:?}")
        };
        assert_eq!(line, "Passed. In the box in both modes.");
        assert_eq!(
            measure,
            Some(125.0),
            "the longest stay, both minutes and between"
        );

        // A minute in Stabilize first does not count for Loiter.
        let (attempt, verdict) = run("hold-the-box", &minute(20, "STABILIZE"));
        assert_eq!((attempt.step, verdict), (0, Verdict::Keep));

        let out = from_home(25.0, 0.0);
        let views: Vec<View> = (20..=26)
            .map(|s| by_hand(at(s, flying(out, 30.0, "LOITER"))))
            .collect();
        let (_, verdict) = run("hold-the-box", &views);
        assert!(
            matches!(verdict, Verdict::Failed { ref line, .. } if line == "Failed. Out of the box for 5 s."),
            "{verdict:?}"
        );
        let (_, verdict) = run("hold-the-box", &[at(20, flying(inside, 30.0, "LOITER"))]);
        assert!(
            matches!(verdict, Verdict::Failed { ref line, .. } if line == "Failed. The controller was released."),
            "{verdict:?}"
        );
    }

    #[test]
    fn through_the_gates_needs_each_line_crossed_in_order_within_the_band() {
        // Across each gate, east to west of its line, 100 m out.
        let across = |bearing: f64, side: f64| {
            let centre = place(
                HOME,
                Offset {
                    bearing_deg: bearing,
                    distance_m: 100.0,
                },
            );
            place(
                centre,
                Offset {
                    bearing_deg: bearing + 90.0,
                    distance_m: side,
                },
            )
        };
        let mut views = Vec::new();
        let mut s = 20;
        for bearing in [0.0, 72.0, 144.0, 216.0, 288.0] {
            for side in [-15.0, 15.0] {
                views.push(by_hand(at(
                    s,
                    flying(across(bearing, side), 30.0, "LOITER"),
                )));
                s += 3;
            }
        }
        let (_, verdict) = run("through-the-gates", &views);
        assert_eq!(
            verdict,
            Verdict::Passed {
                line: "Passed. Through all five in 37 s.".into(),
                measure: Some(37.0)
            }
        );
        // Gate 2 first does not count, nor gate 1 too high.
        let (attempt, verdict) = run("through-the-gates", &views[2..4]);
        assert_eq!((attempt.step, verdict), (0, Verdict::Keep));
        let high: Vec<View> = [-15.0, 15.0]
            .iter()
            .enumerate()
            .map(|(n, &side)| by_hand(at(20 + n as u64, flying(across(0.0, side), 60.0, "LOITER"))))
            .collect();
        let (attempt, verdict) = run("through-the-gates", &high);
        assert_eq!((attempt.step, verdict), (0, Verdict::Keep));
        // Past the gate's end, beside it, is not through it.
        let beside: Vec<View> = [-15.0, 15.0]
            .iter()
            .enumerate()
            .map(|(n, &side)| {
                let wide = place(
                    across(0.0, side),
                    Offset {
                        bearing_deg: 0.0,
                        distance_m: 15.0,
                    },
                );
                by_hand(at(20 + n as u64, flying(wide, 30.0, "LOITER")))
            })
            .collect();
        let (attempt, _) = run("through-the-gates", &beside);
        assert_eq!(attempt.step, 0);
        // Gates 3 and 4 before gate 1: out of order twice.
        let mut s = 20;
        let mut wrong = Vec::new();
        for bearing in [144.0, 216.0] {
            for side in [-15.0, 15.0] {
                wrong.push(by_hand(at(
                    s,
                    flying(across(bearing, side), 30.0, "LOITER"),
                )));
                s += 3;
            }
        }
        let (_, verdict) = run("through-the-gates", &wrong);
        assert!(
            matches!(verdict, Verdict::Failed { ref line, .. } if line == "Failed. Gates crossed out of order twice."),
            "{verdict:?}"
        );
        let (attempt, verdict) = run("through-the-gates", &wrong[..2]);
        assert_eq!(
            (attempt.step, verdict),
            (0, Verdict::Keep),
            "once is not twice"
        );
    }

    #[test]
    fn plan_the_survey_needs_a_mission_of_the_field_flown_in_auto_and_a_landing() {
        // Lines 20 m apart across the field (40 to 160 m east, 40 to 120 m
        // north of home), flown at a point every 10 m.
        let mut lawn = Vec::new();
        for (n, north) in (40..=120).step_by(20).enumerate() {
            let mut line: Vec<(f64, f64)> = (40..=160)
                .step_by(10)
                .map(|east| from_home(f64::from(east), f64::from(north)))
                .collect();
            if n % 2 == 1 {
                line.reverse();
            }
            lawn.extend(line);
        }
        let waypoints: Vec<(f64, f64)> = (40..=120)
            .step_by(20)
            .flat_map(|north| {
                [
                    from_home(40.0, f64::from(north)),
                    from_home(160.0, f64::from(north)),
                ]
            })
            .collect();
        let planned = View {
            mission: Some(waypoints.clone()),
            ..at(20, landed_at(HOME))
        };
        let mut views = vec![planned.clone()];
        let mut s = 30;
        for &point in &lawn {
            views.push(View {
                mission: Some(waypoints.clone()),
                ..at(s, flying(point, 40.0, "AUTO"))
            });
            s += 1;
        }
        views.push(View {
            mission: Some(waypoints.clone()),
            ..at(s + 60, landed_at(HOME))
        });
        let (_, verdict) = run("plan-the-survey", &views);
        let Verdict::Passed { line, measure } = verdict else {
            panic!("{verdict:?}")
        };
        assert_eq!(line, "Passed. Covered 100% of the field.");
        assert_eq!(measure, Some(100.0));

        // The same track in Loiter covers nothing; landed, it is not passed.
        let loiter: Vec<View> = views
            .iter()
            .map(|v| View {
                mode: v.mode.as_ref().map(|_| "LOITER".to_owned()),
                ..v.clone()
            })
            .collect();
        let (attempt, verdict) = run("plan-the-survey", &loiter);
        assert_eq!((attempt.step, verdict), (1, Verdict::Keep));

        // No mission yet keeps it at the first step; one outside fails it.
        let (attempt, _) = run("plan-the-survey", &[at(20, landed_at(HOME))]);
        assert_eq!(attempt.step, 0);
        let mut wide = waypoints;
        wide.push(from_home(190.0, 80.0));
        let (_, verdict) = run(
            "plan-the-survey",
            &[View {
                mission: Some(wide),
                ..at(20, landed_at(HOME))
            }],
        );
        assert!(
            matches!(verdict, Verdict::Failed { ref line, .. } if line == "Failed. A waypoint is more than 20 m outside the field."),
            "{verdict:?}"
        );
    }

    #[test]
    fn a_landing_sequence_planned_is_joined_by_return_to_launch_and_landed_through() {
        let planned = |view: View| View {
            mission_commands: Some(vec![22, 16, 16, 189, 16, 21]),
            ..view
        };
        let away = from_home(1000.0, 0.0);
        let rtl = vec![(31 * S, Command::Rtl)];
        let views = [
            planned(at(20, flying(HOME, 80.0, "LOITER"))),
            planned(at(30, flying(away, 80.0, "GUIDED"))),
            planned(View {
                commands: rtl.clone(),
                ..at(31, flying(away, 80.0, "RTL"))
            }),
            planned(View {
                commands: rtl,
                ..at(200, landed_at(from_home(20.0, 0.0)))
            }),
        ];
        let (_, verdict) = run("land-through-a-landing-sequence", &views);
        assert!(
            matches!(verdict, Verdict::Passed { ref line, .. } if line == "Passed. Landed 20 m from the middle of the ring."),
            "{verdict:?}"
        );

        // Return to launch with no landing sequence planned: failed, and why.
        let (_, verdict) = run(
            "land-through-a-landing-sequence",
            &[View {
                commands: vec![(20 * S, Command::Rtl)],
                ..at(20, flying(HOME, 80.0, "RTL"))
            }],
        );
        assert!(
            matches!(verdict, Verdict::Failed { ref line, .. } if line.contains("no landing sequence to join")),
            "{verdict:?}"
        );
        // A mission without one does not start it.
        let (attempt, _) = run(
            "land-through-a-landing-sequence",
            &[View {
                mission_commands: Some(vec![22, 16, 20]),
                ..at(20, flying(HOME, 80.0, "LOITER"))
            }],
        );
        assert_eq!(attempt.step, 0);
    }

    #[test]
    fn a_fence_uploaded_is_breached_and_brings_the_vehicle_back() {
        let fenced = |view: View| View {
            fence_uploaded: true,
            ..view
        };
        let views = [
            fenced(at(20, flying(HOME, 30.0, "LOITER"))),
            fenced(View {
                fence_breached: Some(true),
                ..at(30, flying(from_home(110.0, 0.0), 30.0, "GUIDED"))
            }),
            fenced(View {
                fence_breached: Some(false),
                ..at(45, flying(from_home(30.0, 0.0), 30.0, "RTL"))
            }),
        ];
        let (_, verdict) = run("fence-breach", &views);
        assert!(matches!(verdict, Verdict::Passed { .. }), "{verdict:?}");
        // No fence: never past the first step.
        let (attempt, _) = run("fence-breach", &[at(20, flying(HOME, 30.0, "LOITER"))]);
        assert_eq!(attempt.step, 0);
        // Landed outside the field: failed.
        let (_, verdict) = run("fence-breach", &[at(20, landed_at(from_home(200.0, 0.0)))]);
        assert!(matches!(verdict, Verdict::Failed { .. }), "{verdict:?}");
    }

    #[test]
    fn return_to_launch_goes_to_the_rally_point_rather_than_home() {
        let spot = from_home(-200.0, 0.0);
        let far = from_home(-450.0, 0.0);
        let rallied = |view: View| View {
            rally: vec![spot],
            ..view
        };
        let rtl = vec![(31 * S, Command::Rtl)];
        let views = [
            rallied(at(20, flying(HOME, 30.0, "LOITER"))),
            rallied(View {
                commands: rtl.clone(),
                ..at(31, flying(far, 30.0, "RTL"))
            }),
            rallied(View {
                commands: rtl.clone(),
                ..at(120, landed_at(spot))
            }),
        ];
        let (_, verdict) = run("rally-home", &views);
        assert!(
            matches!(verdict, Verdict::Passed { ref line, .. } if line == "Passed. Landed 0 m from the rally point."),
            "{verdict:?}"
        );
        // Home instead: failed, with how far.
        let home = [
            rallied(at(20, flying(HOME, 30.0, "LOITER"))),
            rallied(View {
                commands: rtl.clone(),
                ..at(31, flying(far, 30.0, "RTL"))
            }),
            rallied(View {
                commands: rtl,
                ..at(120, landed_at(HOME))
            }),
        ];
        let (_, verdict) = run("rally-home", &home);
        assert!(
            matches!(verdict, Verdict::Failed { ref line, .. } if line == "Failed. Landed 200 m from the rally point."),
            "{verdict:?}"
        );
    }

    #[test]
    fn above_terrain_needs_the_terrain_frame_and_twenty_metres_over_the_ground() {
        let far = from_home(0.0, 600.0);
        let planned = |view: View| View {
            mission_frames: Some(vec![10, 11]),
            clearance_m: Some(30.0),
            ..view
        };
        let views = [
            planned(at(20, flying(HOME, 30.0, "LOITER"))),
            planned(at(60, flying(far, 30.0, "AUTO"))),
            planned(at(120, landed_at(HOME))),
        ];
        let (_, verdict) = run("above-terrain", &views);
        assert!(matches!(verdict, Verdict::Passed { .. }), "{verdict:?}");
        // Above home does not start it.
        let (attempt, _) = run(
            "above-terrain",
            &[View {
                mission_frames: Some(vec![3, 3]),
                ..at(20, flying(HOME, 30.0, "LOITER"))
            }],
        );
        assert_eq!(attempt.step, 0);
        // Down to 12 m over a ridge: failed.
        let low = [
            planned(at(20, flying(HOME, 30.0, "LOITER"))),
            View {
                clearance_m: Some(12.0),
                ..planned(at(40, flying(from_home(0.0, 300.0), 30.0, "AUTO")))
            },
        ];
        let (_, verdict) = run("above-terrain", &low);
        assert!(
            matches!(verdict, Verdict::Failed { ref line, .. } if line.starts_with("Failed. It came within 20 m")),
            "{verdict:?}"
        );
    }

    /// A view of the vehicle doing exactly what a step's checks name, at
    /// `now`: each command sent just then, in the mode, armed or landed,
    /// at the feature.
    fn doing(step: &uxduck_core::domain::practice::Step, attempt: &Attempt, now: u64) -> View {
        let mut view = flying(HOME, 50.0, "GUIDED");
        view.now_us = now;
        for check in &step.all {
            match check {
                Check::CommandSent { command, mode } => view.commands.push((
                    now,
                    match (command.as_str(), mode) {
                        ("setMode", Some(mode)) => Command::SetMode { mode: mode.clone() },
                        ("rtl", _) => Command::Rtl,
                        ("land", _) => Command::Land,
                        ("arm", _) => Command::Arm,
                        ("goTo", _) => Command::GoTo {
                            latitude_deg: HOME.0,
                            longitude_deg: HOME.1,
                            altitude_m: 50.0,
                            altitude_frame: AltitudeFrame::AboveHome,
                        },
                        other => panic!("{other:?}"),
                    },
                )),
                Check::Mode { mode, .. } => view.mode = Some(mode.clone()),
                Check::Landed => {
                    view.armed = false;
                    view.on_ground = Some(true);
                    view.height_m = Some(0.1);
                }
                Check::Armed => view.armed = true,
                Check::Within { feature: id, .. } => {
                    let f = feature(attempt, id).unwrap();
                    view.position = Some((f.latitude_deg, f.longitude_deg));
                }
                other => panic!("a guide's step checks {other:?}"),
            }
        }
        view
    }

    /// Every guide (S117) passes step by step on the commands and state its
    /// steps name, waits on anything else, and never fails, whatever the
    /// vehicle does.
    #[test]
    fn every_guide_ticks_on_what_its_steps_name_and_never_fails() {
        let guides: Vec<&Exercise> = crate::practice::exercises()
            .iter()
            .filter(|e| e.guide)
            .collect();
        assert_eq!(guides.len(), 8);
        for guide in guides {
            let mut attempt = attempt(&guide.id);
            assert!(attempt.guide);
            let mut now = 20 * S;
            for (n, step) in guide.steps.iter().enumerate() {
                // What the vehicle does on its own does not tick a step that
                // asks for a command, nor does what was sent before it began.
                if step
                    .all
                    .iter()
                    .any(|c| matches!(c, Check::CommandSent { .. }))
                {
                    let mut early = doing(step, &attempt, now);
                    for (at, _) in &mut early.commands {
                        *at = attempt.step_started_at_us - 1;
                    }
                    assert_eq!(
                        judge(guide, &attempt, &early),
                        Verdict::Keep,
                        "{}",
                        step.text
                    );
                }
                now += 5 * S;
                let verdict = judge(guide, &attempt, &doing(step, &attempt, now));
                if n + 1 < guide.steps.len() {
                    assert_eq!(
                        verdict,
                        Verdict::Step(attempt.step + 1),
                        "{}: {}",
                        guide.id,
                        step.text
                    );
                    attempt.step += 1;
                    attempt.step_started_at_us = now;
                } else {
                    assert_eq!(
                        verdict,
                        Verdict::Passed {
                            line: guide.passed.clone(),
                            measure: None
                        },
                        "{}",
                        guide.id
                    );
                }
            }
            // Never failed: landed far away, an empty battery, a crash.
            let fresh = super::tests::attempt(&guide.id);
            let far = place(
                HOME,
                Offset {
                    bearing_deg: 10.0,
                    distance_m: 3_000.0,
                },
            );
            for view in [
                landed_at(far),
                View {
                    battery_pct: Some(0.0),
                    ..flying(far, 1.0, "LAND")
                },
                at(10_000, View::default()),
            ] {
                assert!(
                    !matches!(judge(guide, &fresh, &view), Verdict::Failed { .. }),
                    "{}",
                    guide.id
                );
            }
        }
    }

    #[test]
    fn a_quadplanes_own_return_and_landing_modes_count_as_return_and_land() {
        let mode = |m: &str| Command::SetMode { mode: m.into() };
        assert!(is_kind(&mode("QRTL"), "rtl", None));
        assert!(is_kind(&mode("QLAND"), "land", None));
        assert!(is_kind(&mode("SURFACE"), "land", None));
        assert!(!is_kind(&mode("QLOITER"), "land", None));
        assert!(!is_kind(&mode("QLAND"), "rtl", None));
    }

    #[test]
    fn a_hint_is_due_after_its_time() {
        let exercise = exercise("mode-tour").unwrap();
        let attempt = attempt("mode-tour");
        assert_eq!(hint_due(exercise, &attempt, 30 * S), None);
        assert!(hint_due(exercise, &attempt, 71 * S).is_some());
    }
}

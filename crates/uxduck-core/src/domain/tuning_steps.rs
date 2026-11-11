//! Steps and Prepare (S331, ADR-0054 T3, T9). Steps is ArduPilot's tuning
//! process for a controller, each step's state read from the vehicle's
//! parameters. Prepare works out the starting values ArduPilot's "Setting
//! the Aircraft Up for Tuning" gives for the props and battery a person
//! has: its published values for 5 to 30 inch props, taken between those
//! sizes by the logarithm of the size, and held at the ends.

use serde::{Deserialize, Serialize};

use super::tuning::{Controller, Resolved, Term};

/// A battery's chemistry: a cell's full and empty voltages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Chemistry {
    Lipo,
    LipoHv,
    LiIon,
}

impl Chemistry {
    /// Full and empty, per cell, as Mission Planner's Initial Parameter
    /// Setup has them.
    #[must_use]
    pub const fn cell_volts(self) -> (f64, f64) {
        match self {
            Self::Lipo => (4.2, 3.3),
            Self::LipoHv => (4.35, 3.3),
            Self::LiIon => (4.1, 2.8),
        }
    }

    const fn words(self) -> &'static str {
        match self {
            Self::Lipo => "LiPo",
            Self::LipoHv => "LiPo HV",
            Self::LiIon => "Li-ion",
        }
    }
}

/// What the vehicle is: its props and its battery.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Build {
    pub prop_inches: f64,
    pub cells: u32,
    pub chemistry: Chemistry,
}

/// A starting value, and why, in words a person reads beside it.
#[derive(Clone, Debug, PartialEq)]
pub struct Proposal {
    pub name: String,
    pub value: f64,
    pub why: String,
}

/// `MOT_THST_EXPO` for 5, 10 and 20 inch props.
const EXPO: [(f64, f64); 3] = [(5.0, 0.55), (10.0, 0.65), (20.0, 0.75)];
/// `INS_GYRO_FILTER`, Hz.
const GYRO: [(f64, f64); 3] = [(5.0, 80.0), (10.0, 40.0), (20.0, 20.0)];
/// Roll and pitch acceleration limits, deg/s².
const ACCEL: [(f64, f64); 3] = [(10.0, 1100.0), (20.0, 500.0), (30.0, 200.0)];
/// Yaw's, deg/s².
const ACCEL_YAW: [(f64, f64); 3] = [(10.0, 200.0), (20.0, 100.0), (30.0, 90.0)];
/// `INS_ACCEL_FILTER`, Hz.
const ACCEL_FILTER: f64 = 10.0;
/// `ATC_RAT_YAW_FLTE`, Hz.
const YAW_FLTE: f64 = 2.0;
/// Mission Planner's lowest rate filter, Hz.
const RATE_FILTER_MIN: f64 = 10.0;

/// A published value at this prop size: between two sizes by the
/// logarithm of the size, the end's value beyond them.
fn at(points: &[(f64, f64)], inches: f64) -> f64 {
    let (first, last) = (points[0], points[points.len() - 1]);
    if inches <= first.0 {
        return first.1;
    }
    if inches >= last.0 {
        return last.1;
    }
    points
        .windows(2)
        .find(|w| inches <= w[1].0)
        .map_or(last.1, |w| {
            let t = (inches.ln() - w[0].0.ln()) / (w[1].0.ln() - w[0].0.ln());
            w[0].1 + t * (w[1].1 - w[0].1)
        })
}

fn round_to(value: f64, step: f64) -> f64 {
    (value / step).round() * step
}

/// "10 inch props", "5.5 inch props".
fn props(inches: f64) -> String {
    let shown = round_to(inches, 0.1);
    if (shown - shown.round()).abs() < 1e-9 {
        format!("{shown:.0} inch props")
    } else {
        format!("{shown:.1} inch props")
    }
}

const fn prefixes(controller: Controller) -> Option<(&'static str, &'static str)> {
    match controller {
        Controller::Copter => Some(("ATC_", "MOT_")),
        Controller::Hover => Some(("Q_A_", "Q_M_")),
        Controller::Plane => None,
    }
}

/// The starting values for a copter or a VTOL's hover (T9), for the
/// parameters the vehicle has; none for a plane. Each acceleration limit
/// is in the unit its name has: `ACC_` in deg/s², the older `ACCEL_` in
/// centidegrees per second squared.
#[must_use]
pub fn prepare(controller: Controller, build: &Build, has: impl Fn(&str) -> bool) -> Vec<Proposal> {
    let Some((atc, mot)) = prefixes(controller) else {
        return Vec::new();
    };
    let inches = build.prop_inches;
    let size = props(inches);
    let cells = f64::from(build.cells);
    let (full, empty) = build.chemistry.cell_volts();
    let kind = build.chemistry.words();
    let gyro = at(&GYRO, inches).round();
    let filter = (gyro / 2.0).max(RATE_FILTER_MIN);
    let guide = "from ArduPilot's tuning setup";
    let mut out: Vec<Proposal> = Vec::new();
    let mut add = |name: String, value: f64, why: String| {
        if has(&name) {
            out.push(Proposal { name, value, why });
        }
    };
    add(
        format!("{mot}BAT_VOLT_MAX"),
        round_to(cells * full, 0.01),
        format!("{} cells × {full} V: a full {kind}", build.cells),
    );
    add(
        format!("{mot}BAT_VOLT_MIN"),
        round_to(cells * empty, 0.01),
        format!("{} cells × {empty} V: an empty {kind}", build.cells),
    );
    add(
        format!("{mot}THST_EXPO"),
        round_to(at(&EXPO, inches), 0.01),
        format!("About this for {size}, {guide}. A thrust stand measures it better"),
    );
    add(
        "INS_GYRO_FILTER".to_owned(),
        gyro,
        format!("{gyro:.0} Hz for {size}, {guide}"),
    );
    add(
        "INS_ACCEL_FILTER".to_owned(),
        ACCEL_FILTER,
        format!("{ACCEL_FILTER:.0} Hz, {guide}"),
    );
    let accel = round_to(at(&ACCEL, inches), 10.0);
    let accel_yaw = round_to(at(&ACCEL_YAW, inches), 10.0);
    for (axis, limit, what) in [
        ("R", accel, "start a roll"),
        ("P", accel, "start a pitch"),
        ("Y", accel_yaw, "start a turn"),
    ] {
        let why = format!("{limit:.0} deg/s² for {size}: how fast it may {what}, {guide}");
        let now = format!("{atc}ACC_{axis}_MAX");
        if has(&now) {
            add(now, limit, why);
        } else {
            add(format!("{atc}ACCEL_{axis}_MAX"), limit * 100.0, why);
        }
    }
    let half = format!("Half the gyro filter, {filter} Hz");
    for name in ["RLL_FLTD", "RLL_FLTT", "PIT_FLTD", "PIT_FLTT", "YAW_FLTT"] {
        add(format!("{atc}RAT_{name}"), filter, half.clone());
    }
    add(
        format!("{atc}RAT_YAW_FLTE"),
        YAW_FLTE,
        format!("{YAW_FLTE:.0} Hz, {guide}"),
    );
    out
}

/// A step of ArduPilot's tuning process (T3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Step {
    /// Starting values from the props and battery, or a plane's airspeeds.
    Prepare,
    /// A first hover, in which it learns its hover throttle.
    FirstFlight,
    /// The harmonic notch, from the motors' noise.
    Filters,
    /// Tuning in flight by itself, or the gains by hand.
    Tune,
    /// How it is asked to move.
    Feel,
}

/// One step's state, worked out from the vehicle's parameters; advice, it
/// gates nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reading {
    pub step: Step,
    pub done: bool,
    pub why: String,
    /// The parameters it reads, whose last change says when it was done.
    pub names: Vec<String>,
}

/// A reading that says the first words when done, the second when not.
fn reading(step: Step, done: bool, [yes, no]: [&str; 2], names: Vec<String>) -> Reading {
    Reading {
        step,
        done,
        why: (if done { yes } else { no }).to_owned(),
        names,
    }
}

fn differs(value: Option<f64>, default: Option<f64>) -> bool {
    match (value, default) {
        (Some(v), Some(d)) => (v - d).abs() > 1e-6 * d.abs().max(1e-3),
        _ => false,
    }
}

/// The steps for a controller and their states (T3): a copter's or a
/// hover's Prepare, First flight, Filters, Tune and Feel; a plane's
/// Prepare, Tune and Feel. `value` and `default` give a parameter's value
/// now and its default, when the vehicle has it.
#[must_use]
pub fn steps(
    resolved: &Resolved,
    value: impl Fn(&str) -> Option<f64>,
    default: impl Fn(&str) -> Option<f64>,
) -> Vec<Reading> {
    let changed = |name: &str| differs(value(name), default(name));
    let gains: Vec<String> = resolved
        .rates
        .iter()
        .flat_map(|r| r.terms.iter())
        .filter(|t| matches!(t.role, Term::P | Term::I | Term::D | Term::Ff))
        .map(|t| t.name.clone())
        .collect();
    let feel: Vec<String> = resolved.feel.iter().map(|f| f.name.clone()).collect();
    let tune = reading(
        Step::Tune,
        gains.iter().any(|n| changed(n)),
        [
            "The gains are not their defaults",
            "The gains are their defaults: AutoTune, or tune them by hand",
        ],
        gains,
    );
    let feel = reading(
        Step::Feel,
        feel.iter().any(|n| changed(n)),
        [
            "Changed from the defaults",
            "At the defaults: set it to how you like to fly",
        ],
        feel,
    );
    let Some((_, mot)) = prefixes(resolved.controller) else {
        let speeds: Vec<String> = [
            "AIRSPEED_MIN",
            "AIRSPEED_MAX",
            "ARSPD_FBW_MIN",
            "ARSPD_FBW_MAX",
            "SCALING_SPEED",
        ]
        .into_iter()
        .filter(|n| value(n).is_some())
        .map(str::to_owned)
        .collect();
        let prepare = reading(
            Step::Prepare,
            speeds.iter().any(|n| changed(n)),
            [
                "Its airspeed limits are set",
                "Its airspeed limits are the defaults: set its stall and top speeds, and check its control surfaces move the right way",
            ],
            speeds,
        );
        return vec![prepare, tune, feel];
    };
    let max = format!("{mot}BAT_VOLT_MAX");
    let min = format!("{mot}BAT_VOLT_MIN");
    // Set means changed from its default, or above 0 where it is unknown.
    let battery = [&max, &min].iter().all(|n| match default(n) {
        Some(_) => changed(n),
        None => value(n).is_some_and(|v| v > 0.0),
    });
    let build = Build {
        prop_inches: 10.0,
        cells: 4,
        chemistry: Chemistry::Lipo,
    };
    let prepare = reading(
        Step::Prepare,
        battery,
        [
            "Its battery voltages for thrust are set",
            "Its battery voltages for thrust are not set yet",
        ],
        prepare(resolved.controller, &build, |n| value(n).is_some())
            .into_iter()
            .map(|p| p.name)
            .collect(),
    );
    let hover = format!("{mot}THST_HOVER");
    let learned = changed(&hover);
    let first = Reading {
        step: Step::FirstFlight,
        done: learned,
        why: match value(&hover) {
            Some(v) if learned => format!("It has learned to hover at {:.0}% throttle", v * 100.0),
            _ => "Hover in AltHold for a minute: it learns its hover throttle".to_owned(),
        },
        names: vec![hover],
    };
    let filters = reading(
        Step::Filters,
        value("INS_HNTCH_ENABLE").is_some_and(|v| v >= 1.0),
        [
            "The harmonic notch is on",
            "The harmonic notch is off: it filters out the motors' noise",
        ],
        vec!["INS_HNTCH_ENABLE".to_owned()],
    );
    vec![prepare, first, filters, tune, feel]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(prop_inches: f64) -> Build {
        Build {
            prop_inches,
            cells: 4,
            chemistry: Chemistry::Lipo,
        }
    }

    fn value_of(proposals: &[Proposal], name: &str) -> f64 {
        proposals
            .iter()
            .find(|p| p.name == name)
            .unwrap_or_else(|| panic!("{name} is proposed"))
            .value
    }

    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn gives_the_documented_values_for_5_10_and_20_inch_props() {
        let all = |_: &str| true;
        let five = prepare(Controller::Copter, &build(5.0), all);
        assert!(near(value_of(&five, "MOT_THST_EXPO"), 0.55));
        assert!(near(value_of(&five, "INS_GYRO_FILTER"), 80.0));
        let ten = prepare(Controller::Copter, &build(10.0), all);
        assert!(near(value_of(&ten, "MOT_THST_EXPO"), 0.65));
        assert!(near(value_of(&ten, "INS_GYRO_FILTER"), 40.0));
        assert!(near(value_of(&ten, "ATC_ACC_R_MAX"), 1100.0));
        assert!(near(value_of(&ten, "ATC_ACC_P_MAX"), 1100.0));
        assert!(near(value_of(&ten, "ATC_ACC_Y_MAX"), 200.0));
        assert!(near(value_of(&ten, "ATC_RAT_RLL_FLTD"), 20.0));
        assert!(near(value_of(&ten, "ATC_RAT_YAW_FLTE"), 2.0));
        assert!(near(value_of(&ten, "INS_ACCEL_FILTER"), 10.0));
        let twenty = prepare(Controller::Copter, &build(20.0), all);
        assert!(near(value_of(&twenty, "MOT_THST_EXPO"), 0.75));
        assert!(near(value_of(&twenty, "INS_GYRO_FILTER"), 20.0));
        assert!(near(value_of(&twenty, "ATC_ACC_R_MAX"), 500.0));
        assert!(near(value_of(&twenty, "ATC_ACC_Y_MAX"), 100.0));
        // The rate filters never go below 10 Hz.
        assert!(near(value_of(&twenty, "ATC_RAT_PIT_FLTT"), 10.0));
        let thirty = prepare(Controller::Copter, &build(30.0), all);
        assert!(near(value_of(&thirty, "ATC_ACC_R_MAX"), 200.0));
        assert!(near(value_of(&thirty, "ATC_ACC_Y_MAX"), 90.0));
    }

    #[test]
    fn takes_sizes_between_by_their_logarithm_and_holds_the_ends() {
        let all = |_: &str| true;
        let seven = prepare(Controller::Copter, &build(7.0), all);
        assert!(near(value_of(&seven, "INS_GYRO_FILTER"), 61.0));
        let three = prepare(Controller::Copter, &build(3.0), all);
        assert!(near(value_of(&three, "INS_GYRO_FILTER"), 80.0));
        assert!(near(value_of(&three, "ATC_ACC_R_MAX"), 1100.0));
    }

    #[test]
    fn works_the_battery_out_from_its_cells_and_chemistry() {
        let all = |_: &str| true;
        let lipo = prepare(Controller::Copter, &build(10.0), all);
        assert!(near(value_of(&lipo, "MOT_BAT_VOLT_MAX"), 16.8));
        assert!(near(value_of(&lipo, "MOT_BAT_VOLT_MIN"), 13.2));
        let hv = Build {
            cells: 6,
            chemistry: Chemistry::LipoHv,
            ..build(10.0)
        };
        let hv = prepare(Controller::Copter, &hv, all);
        assert!(near(value_of(&hv, "MOT_BAT_VOLT_MAX"), 26.1));
        let ion = Build {
            chemistry: Chemistry::LiIon,
            ..build(10.0)
        };
        let ion = prepare(Controller::Copter, &ion, all);
        assert!(near(value_of(&ion, "MOT_BAT_VOLT_MIN"), 11.2));
        assert_eq!(
            lipo.iter()
                .find(|p| p.name == "MOT_BAT_VOLT_MAX")
                .map(|p| p.why.as_str()),
            Some("4 cells × 4.2 V: a full LiPo")
        );
    }

    #[test]
    fn reads_each_steps_state_from_the_parameters_and_their_defaults() {
        use std::collections::BTreeMap;
        let mut values: BTreeMap<&str, f64> = [
            ("MOT_BAT_VOLT_MAX", 12.8),
            ("MOT_BAT_VOLT_MIN", 9.6),
            ("MOT_THST_HOVER", 0.35),
            ("INS_HNTCH_ENABLE", 0.0),
            ("ATC_RAT_RLL_P", 0.135),
            ("ATC_INPUT_TC", 0.15),
        ]
        .into_iter()
        .collect();
        let defaults = values.clone();
        let resolved =
            super::super::tuning::resolve(Controller::Copter, |n| values.contains_key(n))
                .expect("a copter");
        let read = |values: &BTreeMap<&str, f64>| {
            steps(
                &resolved,
                |n| values.get(n).copied(),
                |n| defaults.get(n).copied(),
            )
        };
        let fresh = read(&values);
        let order: Vec<Step> = fresh.iter().map(|r| r.step).collect();
        assert_eq!(
            order,
            [
                Step::Prepare,
                Step::FirstFlight,
                Step::Filters,
                Step::Tune,
                Step::Feel
            ]
        );
        assert!(fresh.iter().all(|r| !r.done), "{fresh:?}");
        values.insert("MOT_BAT_VOLT_MAX", 16.8);
        values.insert("MOT_BAT_VOLT_MIN", 13.2);
        values.insert("MOT_THST_HOVER", 0.21);
        values.insert("ATC_RAT_RLL_P", 0.1);
        let after = read(&values);
        assert!(after[0].done && after[1].done && after[3].done, "{after:?}");
        assert_eq!(after[1].why, "It has learned to hover at 21% throttle");
        assert!(!after[2].done && !after[4].done);
    }

    #[test]
    fn uses_the_names_the_vehicle_has_and_their_units() {
        let old = |name: &str| !name.contains("_ACC_");
        let older = prepare(Controller::Copter, &build(10.0), old);
        assert!(near(value_of(&older, "ATC_ACCEL_R_MAX"), 110_000.0));
        assert!(older.iter().all(|p| !p.name.contains("_ACC_")));
        let hover = prepare(Controller::Hover, &build(10.0), |n| {
            n.starts_with("Q_") || n.starts_with("INS_")
        });
        assert!(near(value_of(&hover, "Q_M_BAT_VOLT_MAX"), 16.8));
        assert!(near(value_of(&hover, "Q_A_ACC_R_MAX"), 1100.0));
        assert!(prepare(Controller::Plane, &build(10.0), |_| true).is_empty());
    }
}

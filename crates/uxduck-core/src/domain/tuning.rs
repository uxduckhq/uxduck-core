//! Tuning (ADR-0054 T8): which of a vehicle's parameters fill each part of
//! its Tuning tab. A part (a role) lists the names it has had, newest first,
//! for each controller; the vehicle's own parameters decide which one it is.
//! A role with none of its names on the vehicle is left out, never guessed.
//! Names changed between releases, and units with them (Copter 4.7's
//! `ATC_ANGLE_MAX` in degrees was `ANGLE_MAX` in centidegrees), so the
//! value is always read in the unit its own definition gives.

use serde::{Deserialize, Serialize};

use super::param_definitions::Firmware;

/// A set of controllers a vehicle is tuned by. A VTOL has two: its hover
/// (a quadplane's `Q_A_` controllers) and its plane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Controller {
    /// Copter's multicopter controllers (`ATC_`).
    Copter,
    /// A quadplane's hover (`Q_A_`): the same controllers as a copter's.
    Hover,
    /// Plane's fixed-wing rate controllers (`RLL_RATE_` and the rest).
    Plane,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Axis {
    Roll,
    Pitch,
    Yaw,
}

impl Axis {
    pub const ALL: [Self; 3] = [Self::Roll, Self::Pitch, Self::Yaw];

    /// As a copter's parameters spell it: `RLL`, `PIT`, `YAW`.
    const fn copter(self) -> &'static str {
        match self {
            Self::Roll => "RLL",
            Self::Pitch => "PIT",
            Self::Yaw => "YAW",
        }
    }

    /// A plane's rate controller: `RLL_RATE_`, `PTCH_RATE_`, `YAW_RATE_`.
    const fn plane(self) -> &'static str {
        match self {
            Self::Roll => "RLL_RATE_",
            Self::Pitch => "PTCH_RATE_",
            Self::Yaw => "YAW_RATE_",
        }
    }
}

/// One term of a rate controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Term {
    P,
    I,
    D,
    Ff,
    /// The most the I term may add.
    Imax,
    /// The target filter.
    Fltt,
    /// The D term's filter.
    Fltd,
    /// The error filter.
    Flte,
}

impl Term {
    pub const ALL: [Self; 8] = [
        Self::P,
        Self::I,
        Self::D,
        Self::Ff,
        Self::Imax,
        Self::Fltt,
        Self::Fltd,
        Self::Flte,
    ];

    const fn suffix(self) -> &'static str {
        match self {
            Self::P => "P",
            Self::I => "I",
            Self::D => "D",
            Self::Ff => "FF",
            Self::Imax => "IMAX",
            Self::Fltt => "FLTT",
            Self::Fltd => "FLTD",
            Self::Flte => "FLTE",
        }
    }
}

/// How the vehicle is asked to move (T4): never a gain, so changing one
/// cannot make it oscillate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Feel {
    /// How quickly it follows the sticks: a time constant, smaller is sharper.
    Responsiveness,
    /// The most it leans.
    MaxLean,
    MaxRollRate,
    MaxPitchRate,
    MaxYawRate,
    ClimbSpeed,
    DescentSpeed,
    /// A plane's most bank.
    RollLimit,
    PitchUpLimit,
    PitchDownLimit,
    /// A plane's time to reach the angle asked for: smaller is sharper.
    RollResponse,
    PitchResponse,
}

/// A role's names, newest first.
type Names = &'static [&'static str];

const COPTER_FEEL: [(Feel, Names); 7] = [
    (Feel::Responsiveness, &["ATC_INPUT_TC"]),
    (Feel::MaxLean, &["ATC_ANGLE_MAX", "ANGLE_MAX"]),
    (Feel::MaxRollRate, &["ATC_RATE_R_MAX"]),
    (Feel::MaxPitchRate, &["ATC_RATE_P_MAX"]),
    (Feel::MaxYawRate, &["ATC_RATE_Y_MAX"]),
    (Feel::ClimbSpeed, &["PILOT_SPD_UP", "PILOT_SPEED_UP"]),
    (Feel::DescentSpeed, &["PILOT_SPD_DN", "PILOT_SPEED_DN"]),
];

const HOVER_FEEL: [(Feel, Names); 7] = [
    (Feel::Responsiveness, &["Q_A_INPUT_TC"]),
    (Feel::MaxLean, &["Q_A_ANGLE_MAX", "Q_ANGLE_MAX"]),
    (Feel::MaxRollRate, &["Q_A_RATE_R_MAX"]),
    (Feel::MaxPitchRate, &["Q_A_RATE_P_MAX"]),
    (Feel::MaxYawRate, &["Q_A_RATE_Y_MAX"]),
    (Feel::ClimbSpeed, &["Q_PILOT_SPD_UP", "Q_VELZ_MAX"]),
    (Feel::DescentSpeed, &["Q_PILOT_SPD_DN", "Q_VELZ_MAX_DN"]),
];

const PLANE_FEEL: [(Feel, Names); 5] = [
    (Feel::RollLimit, &["ROLL_LIMIT_DEG", "LIM_ROLL_CD"]),
    (Feel::PitchUpLimit, &["PTCH_LIM_MAX_DEG", "LIM_PITCH_MAX"]),
    (Feel::PitchDownLimit, &["PTCH_LIM_MIN_DEG", "LIM_PITCH_MIN"]),
    (Feel::RollResponse, &["RLL2SRV_TCONST"]),
    (Feel::PitchResponse, &["PTCH2SRV_TCONST"]),
];

impl Controller {
    const fn feel(self) -> &'static [(Feel, Names)] {
        match self {
            Self::Copter => &COPTER_FEEL,
            Self::Hover => &HOVER_FEEL,
            Self::Plane => &PLANE_FEEL,
        }
    }

    /// A rate controller's term: `ATC_RAT_RLL_P`, `Q_A_RAT_RLL_P`, `RLL_RATE_P`.
    fn rate(self, axis: Axis, term: Term) -> String {
        match self {
            Self::Copter => format!("ATC_RAT_{}_{}", axis.copter(), term.suffix()),
            Self::Hover => format!("Q_A_RAT_{}_{}", axis.copter(), term.suffix()),
            Self::Plane => format!("{}{}", axis.plane(), term.suffix()),
        }
    }

    /// The angle controller's P, which a plane has none of (its time
    /// constants are in Feel).
    fn angle(self, axis: Axis) -> Option<String> {
        match self {
            Self::Copter => Some(format!("ATC_ANG_{}_P", axis.copter())),
            Self::Hover => Some(format!("Q_A_ANG_{}_P", axis.copter())),
            Self::Plane => None,
        }
    }
}

/// Why a vehicle has no Tuning tab yet (T14).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Unsupported {
    /// Not ArduPilot: PX4 comes with its own driver.
    Autopilot,
    /// A traditional helicopter, tuned its own way.
    Helicopter,
    /// Rovers, boats, submarines, blimps and trackers.
    VehicleType,
}

/// Which controllers a vehicle is tuned by, from its autopilot and its
/// heartbeat's `MAV_TYPE`: a plane may also hover, which its parameters say.
///
/// # Errors
/// When UXDuck does not tune this kind of vehicle yet.
pub fn controllers(autopilot: &str, vehicle_type: &str) -> Result<Vec<Controller>, Unsupported> {
    if autopilot != "Ardupilotmega" {
        return Err(Unsupported::Autopilot);
    }
    if vehicle_type == "Helicopter" {
        return Err(Unsupported::Helicopter);
    }
    match Firmware::of(vehicle_type) {
        Some(Firmware::ArduCopter) => Ok(vec![Controller::Copter]),
        Some(Firmware::ArduPlane) => Ok(vec![Controller::Hover, Controller::Plane]),
        _ => Err(Unsupported::VehicleType),
    }
}

/// A role filled by one of the vehicle's parameters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filled<R> {
    pub role: R,
    pub name: String,
}

/// One rate controller's terms that the vehicle has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rate {
    pub axis: Axis,
    pub terms: Vec<Filled<Term>>,
}

/// One controller's parts, each with the vehicle's parameter for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub controller: Controller,
    pub feel: Vec<Filled<Feel>>,
    pub rates: Vec<Rate>,
    pub angles: Vec<Filled<Axis>>,
}

impl Resolved {
    /// Every parameter it names: what a snapshot keeps (T5).
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.feel
            .iter()
            .map(|f| f.name.as_str())
            .chain(
                self.rates
                    .iter()
                    .flat_map(|r| r.terms.iter().map(|t| t.name.as_str())),
            )
            .chain(self.angles.iter().map(|a| a.name.as_str()))
    }

    fn is_empty(&self) -> bool {
        self.feel.is_empty() && self.rates.is_empty() && self.angles.is_empty()
    }
}

/// The controller's parts the vehicle has, by its parameters' names; None
/// when it has none of them (a plane that does not hover).
pub fn resolve(controller: Controller, has: impl Fn(&str) -> bool) -> Option<Resolved> {
    let feel = controller
        .feel()
        .iter()
        .filter_map(|(role, names)| {
            names.iter().find(|n| has(n)).map(|name| Filled {
                role: *role,
                name: (*name).to_owned(),
            })
        })
        .collect();
    let rates = Axis::ALL
        .into_iter()
        .map(|axis| Rate {
            axis,
            terms: Term::ALL
                .into_iter()
                .map(|term| (term, controller.rate(axis, term)))
                .filter(|(_, name)| has(name))
                .map(|(role, name)| Filled { role, name })
                .collect(),
        })
        .filter(|rate| !rate.terms.is_empty())
        .collect();
    let angles = Axis::ALL
        .into_iter()
        .filter_map(|axis| {
            controller
                .angle(axis)
                .filter(|name| has(name))
                .map(|name| Filled { role: axis, name })
        })
        .collect();
    let resolved = Resolved {
        controller,
        feel,
        rates,
        angles,
    };
    (!resolved.is_empty()).then_some(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names_of(list: &[&str]) -> impl Fn(&str) -> bool {
        let list: Vec<String> = list.iter().map(|s| (*s).to_owned()).collect();
        move |name| list.iter().any(|n| n == name)
    }

    #[test]
    fn a_copter_before_4_7_has_its_old_names() {
        let has = names_of(&[
            "ANGLE_MAX",
            "ATC_INPUT_TC",
            "ATC_RAT_RLL_P",
            "PILOT_SPEED_UP",
        ]);
        let copter = resolve(Controller::Copter, has).unwrap();
        let lean = copter
            .feel
            .iter()
            .find(|f| f.role == Feel::MaxLean)
            .unwrap();
        assert_eq!(lean.name, "ANGLE_MAX");
        let climb = copter
            .feel
            .iter()
            .find(|f| f.role == Feel::ClimbSpeed)
            .unwrap();
        assert_eq!(climb.name, "PILOT_SPEED_UP");
        assert_eq!(copter.rates.len(), 1);
        assert_eq!(copter.rates[0].terms[0].name, "ATC_RAT_RLL_P");
    }

    #[test]
    fn the_newest_name_wins_when_both_are_there() {
        let has = names_of(&["ANGLE_MAX", "ATC_ANGLE_MAX"]);
        let copter = resolve(Controller::Copter, has).unwrap();
        assert_eq!(copter.feel[0].name, "ATC_ANGLE_MAX");
    }

    #[test]
    fn a_role_the_vehicle_lacks_is_left_out_and_nothing_is_guessed() {
        let has = names_of(&["RLL_RATE_P", "RLL_RATE_I", "PTCH_RATE_P"]);
        let plane = resolve(Controller::Plane, has).unwrap();
        assert!(plane.feel.is_empty());
        assert!(plane.angles.is_empty());
        assert_eq!(
            plane.rates.iter().map(|r| r.axis).collect::<Vec<_>>(),
            [Axis::Roll, Axis::Pitch]
        );
        assert_eq!(plane.names().count(), 3);
    }

    #[test]
    fn a_plane_that_does_not_hover_has_no_hover() {
        assert_eq!(resolve(Controller::Hover, names_of(&["RLL_RATE_P"])), None);
    }

    #[test]
    fn a_vtol_hovers_with_its_q_controllers() {
        let has = names_of(&["Q_A_RAT_PIT_D", "Q_A_ANG_YAW_P", "Q_VELZ_MAX"]);
        let hover = resolve(Controller::Hover, has).unwrap();
        assert_eq!(hover.rates[0].axis, Axis::Pitch);
        assert_eq!(hover.rates[0].terms[0].role, Term::D);
        assert_eq!(hover.angles[0].role, Axis::Yaw);
        assert_eq!(hover.feel[0].role, Feel::ClimbSpeed);
    }

    #[test]
    fn which_vehicles_are_tuned() {
        assert_eq!(
            controllers("Ardupilotmega", "Quadrotor"),
            Ok(vec![Controller::Copter])
        );
        assert_eq!(
            controllers("Ardupilotmega", "VtolQuadrotor"),
            Ok(vec![Controller::Hover, Controller::Plane])
        );
        assert_eq!(
            controllers("Ardupilotmega", "FixedWing"),
            Ok(vec![Controller::Hover, Controller::Plane])
        );
        assert_eq!(
            controllers("Ardupilotmega", "Helicopter"),
            Err(Unsupported::Helicopter)
        );
        assert_eq!(
            controllers("Ardupilotmega", "GroundRover"),
            Err(Unsupported::VehicleType)
        );
        assert_eq!(controllers("Px4", "Quadrotor"), Err(Unsupported::Autopilot));
    }
}

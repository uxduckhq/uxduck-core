//! Tuning (ADR-0054 T8): which of a vehicle's parameters fill each part of
//! its Tuning tab. A part (a role) lists the names it has had, newest first,
//! for each controller; the vehicle's own parameters decide which one it is.
//! A role with none of its names on the vehicle is left out, never guessed.
//! Names changed between releases, and units with them (Copter 4.7's
//! `ATC_ANGLE_MAX` in degrees was `ANGLE_MAX` in centidegrees), so the
//! value is always read in the unit its own definition gives. PX4 fills the
//! same parts with its own names (T14): `MC_` for a multicopter and a
//! VTOL's hover, `FW_` for a plane.

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

/// Whose parameters fill the parts: they are named differently, and only
/// ArduPilot's come with Steps and AutoTune.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Autopilot {
    ArduPilot,
    Px4,
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

    /// As PX4's multicopter parameters spell it: `MC_ROLLRATE_P`, `MC_ROLL_P`.
    const fn px4(self) -> &'static str {
        match self {
            Self::Roll => "ROLL",
            Self::Pitch => "PITCH",
            Self::Yaw => "YAW",
        }
    }

    /// PX4's short rate controller name: `MC_RR_INT_LIM`, `FW_RR_P`.
    const fn px4_rate(self) -> &'static str {
        match self {
            Self::Roll => "RR",
            Self::Pitch => "PR",
            Self::Yaw => "YR",
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

/// PX4's multicopter, for a copter and a VTOL's hover alike.
const PX4_MC_FEEL: [(Feel, Names); 7] = [
    (Feel::Responsiveness, &["MC_MAN_TILT_TAU"]),
    (Feel::MaxLean, &["MPC_MAN_TILT_MAX"]),
    (Feel::MaxRollRate, &["MC_ROLLRATE_MAX"]),
    (Feel::MaxPitchRate, &["MC_PITCHRATE_MAX"]),
    (Feel::MaxYawRate, &["MC_YAWRATE_MAX"]),
    (Feel::ClimbSpeed, &["MPC_Z_VEL_MAX_UP"]),
    (Feel::DescentSpeed, &["MPC_Z_VEL_MAX_DN"]),
];

const PX4_FW_FEEL: [(Feel, Names); 5] = [
    (Feel::RollLimit, &["FW_R_LIM"]),
    (Feel::PitchUpLimit, &["FW_P_LIM_MAX"]),
    (Feel::PitchDownLimit, &["FW_P_LIM_MIN"]),
    (Feel::RollResponse, &["FW_R_TC"]),
    (Feel::PitchResponse, &["FW_P_TC"]),
];

impl Controller {
    const fn feel(self, autopilot: Autopilot) -> &'static [(Feel, Names)] {
        match (autopilot, self) {
            (Autopilot::ArduPilot, Self::Copter) => &COPTER_FEEL,
            (Autopilot::ArduPilot, Self::Hover) => &HOVER_FEEL,
            (Autopilot::ArduPilot, Self::Plane) => &PLANE_FEEL,
            (Autopilot::Px4, Self::Copter | Self::Hover) => &PX4_MC_FEEL,
            (Autopilot::Px4, Self::Plane) => &PX4_FW_FEEL,
        }
    }

    /// A rate controller's term: `ATC_RAT_RLL_P`, `Q_A_RAT_RLL_P`,
    /// `RLL_RATE_P`, or PX4's `MC_ROLLRATE_P` and `FW_RR_P`. PX4 filters
    /// the gyro for every axis at once, so it has no term filters.
    fn rate(self, autopilot: Autopilot, axis: Axis, term: Term) -> Option<String> {
        match (autopilot, self) {
            (Autopilot::ArduPilot, Self::Copter) => {
                Some(format!("ATC_RAT_{}_{}", axis.copter(), term.suffix()))
            }
            (Autopilot::ArduPilot, Self::Hover) => {
                Some(format!("Q_A_RAT_{}_{}", axis.copter(), term.suffix()))
            }
            (Autopilot::ArduPilot, Self::Plane) => {
                Some(format!("{}{}", axis.plane(), term.suffix()))
            }
            (Autopilot::Px4, Self::Copter | Self::Hover) => match term {
                Term::P | Term::I | Term::D | Term::Ff => {
                    Some(format!("MC_{}RATE_{}", axis.px4(), term.suffix()))
                }
                Term::Imax => Some(format!("MC_{}_INT_LIM", axis.px4_rate())),
                Term::Fltt | Term::Fltd | Term::Flte => None,
            },
            (Autopilot::Px4, Self::Plane) => match term {
                Term::P | Term::I | Term::D | Term::Ff | Term::Imax => {
                    Some(format!("FW_{}_{}", axis.px4_rate(), term.suffix()))
                }
                Term::Fltt | Term::Fltd | Term::Flte => None,
            },
        }
    }

    /// The angle controller's P, which a plane has none of (its time
    /// constants are in Feel).
    fn angle(self, autopilot: Autopilot, axis: Axis) -> Option<String> {
        match (autopilot, self) {
            (Autopilot::ArduPilot, Self::Copter) => Some(format!("ATC_ANG_{}_P", axis.copter())),
            (Autopilot::ArduPilot, Self::Hover) => Some(format!("Q_A_ANG_{}_P", axis.copter())),
            (Autopilot::Px4, Self::Copter | Self::Hover) => Some(format!("MC_{}_P", axis.px4())),
            (_, Self::Plane) => None,
        }
    }
}

/// Why a vehicle has no Tuning tab yet (T14).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Unsupported {
    /// Neither ArduPilot nor PX4.
    Autopilot,
    /// A traditional helicopter, tuned its own way.
    Helicopter,
    /// Rovers, boats, submarines, blimps and trackers.
    VehicleType,
}

/// Which controllers a vehicle is tuned by, from its autopilot and its
/// heartbeat's `MAV_TYPE`. An ArduPilot plane may also hover, which its
/// parameters say. PX4 has every controller's parameters whatever it flies,
/// so its `MAV_TYPE` alone says whether it hovers: only a VTOL does.
///
/// # Errors
/// When UXDuck does not tune this kind of vehicle yet.
pub fn controllers(
    autopilot: &str,
    vehicle_type: &str,
) -> Result<(Autopilot, Vec<Controller>), Unsupported> {
    let autopilot = match autopilot {
        "Ardupilotmega" => Autopilot::ArduPilot,
        "Px4" => Autopilot::Px4,
        _ => return Err(Unsupported::Autopilot),
    };
    if vehicle_type == "Helicopter" {
        return Err(Unsupported::Helicopter);
    }
    let controllers = match (autopilot, Firmware::of(vehicle_type)) {
        (_, Some(Firmware::ArduCopter)) => vec![Controller::Copter],
        (Autopilot::Px4, Some(Firmware::ArduPlane)) if !vehicle_type.starts_with("Vtol") => {
            vec![Controller::Plane]
        }
        (_, Some(Firmware::ArduPlane)) => vec![Controller::Hover, Controller::Plane],
        _ => return Err(Unsupported::VehicleType),
    };
    Ok((autopilot, controllers))
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
    pub autopilot: Autopilot,
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
pub fn resolve(
    controller: Controller,
    autopilot: Autopilot,
    has: impl Fn(&str) -> bool,
) -> Option<Resolved> {
    let feel = controller
        .feel(autopilot)
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
                .filter_map(|term| Some((term, controller.rate(autopilot, axis, term)?)))
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
                .angle(autopilot, axis)
                .filter(|name| has(name))
                .map(|name| Filled { role: axis, name })
        })
        .collect();
    let resolved = Resolved {
        controller,
        autopilot,
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
        let copter = resolve(Controller::Copter, Autopilot::ArduPilot, has).unwrap();
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
        let copter = resolve(Controller::Copter, Autopilot::ArduPilot, has).unwrap();
        assert_eq!(copter.feel[0].name, "ATC_ANGLE_MAX");
    }

    #[test]
    fn a_role_the_vehicle_lacks_is_left_out_and_nothing_is_guessed() {
        let has = names_of(&["RLL_RATE_P", "RLL_RATE_I", "PTCH_RATE_P"]);
        let plane = resolve(Controller::Plane, Autopilot::ArduPilot, has).unwrap();
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
        assert_eq!(
            resolve(
                Controller::Hover,
                Autopilot::ArduPilot,
                names_of(&["RLL_RATE_P"])
            ),
            None
        );
    }

    #[test]
    fn a_vtol_hovers_with_its_q_controllers() {
        let has = names_of(&["Q_A_RAT_PIT_D", "Q_A_ANG_YAW_P", "Q_VELZ_MAX"]);
        let hover = resolve(Controller::Hover, Autopilot::ArduPilot, has).unwrap();
        assert_eq!(hover.rates[0].axis, Axis::Pitch);
        assert_eq!(hover.rates[0].terms[0].role, Term::D);
        assert_eq!(hover.angles[0].role, Axis::Yaw);
        assert_eq!(hover.feel[0].role, Feel::ClimbSpeed);
    }

    #[test]
    fn which_vehicles_are_tuned() {
        use Autopilot::{ArduPilot, Px4};
        assert_eq!(
            controllers("Ardupilotmega", "Quadrotor"),
            Ok((ArduPilot, vec![Controller::Copter]))
        );
        assert_eq!(
            controllers("Ardupilotmega", "VtolQuadrotor"),
            Ok((ArduPilot, vec![Controller::Hover, Controller::Plane]))
        );
        assert_eq!(
            controllers("Ardupilotmega", "FixedWing"),
            Ok((ArduPilot, vec![Controller::Hover, Controller::Plane]))
        );
        assert_eq!(
            controllers("Px4", "Quadrotor"),
            Ok((Px4, vec![Controller::Copter]))
        );
        assert_eq!(
            controllers("Px4", "VtolFixedrotor"),
            Ok((Px4, vec![Controller::Hover, Controller::Plane]))
        );
        assert_eq!(
            controllers("Px4", "FixedWing"),
            Ok((Px4, vec![Controller::Plane]))
        );
        assert_eq!(
            controllers("Px4", "Helicopter"),
            Err(Unsupported::Helicopter)
        );
        assert_eq!(
            controllers("Px4", "GroundRover"),
            Err(Unsupported::VehicleType)
        );
        assert_eq!(
            controllers("Ardupilotmega", "Helicopter"),
            Err(Unsupported::Helicopter)
        );
        assert_eq!(
            controllers("Ardupilotmega", "GroundRover"),
            Err(Unsupported::VehicleType)
        );
        assert_eq!(
            controllers("Slugs", "Quadrotor"),
            Err(Unsupported::Autopilot)
        );
    }

    /// PX4 1.17's names, as its parameter metadata lists them.
    #[test]
    fn px4_fills_the_parts_with_its_own_names() {
        let has = names_of(&[
            "MC_ROLLRATE_P",
            "MC_ROLLRATE_I",
            "MC_ROLLRATE_D",
            "MC_ROLLRATE_FF",
            "MC_RR_INT_LIM",
            "MC_YAWRATE_P",
            "MC_PITCH_P",
            "MC_MAN_TILT_TAU",
            "MPC_MAN_TILT_MAX",
            "MPC_Z_VEL_MAX_DN",
            "FW_PR_P",
            "FW_PR_IMAX",
            "FW_R_TC",
            "ATC_RAT_RLL_P",
        ]);
        let copter = resolve(Controller::Copter, Autopilot::Px4, &has).unwrap();
        assert_eq!(copter.autopilot, Autopilot::Px4);
        let roll: Vec<(Term, &str)> = copter.rates[0]
            .terms
            .iter()
            .map(|t| (t.role, t.name.as_str()))
            .collect();
        assert_eq!(
            roll,
            [
                (Term::P, "MC_ROLLRATE_P"),
                (Term::I, "MC_ROLLRATE_I"),
                (Term::D, "MC_ROLLRATE_D"),
                (Term::Ff, "MC_ROLLRATE_FF"),
                (Term::Imax, "MC_RR_INT_LIM")
            ]
        );
        assert_eq!(copter.rates[1].axis, Axis::Yaw);
        assert_eq!(copter.angles[0].name, "MC_PITCH_P");
        assert_eq!(
            copter
                .feel
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            ["MC_MAN_TILT_TAU", "MPC_MAN_TILT_MAX", "MPC_Z_VEL_MAX_DN"]
        );
        let plane = resolve(Controller::Plane, Autopilot::Px4, &has).unwrap();
        assert_eq!(
            plane.names().collect::<Vec<_>>(),
            ["FW_R_TC", "FW_PR_P", "FW_PR_IMAX"]
        );
        assert!(!copter.names().any(|n| n.starts_with("ATC_")));
        let ardupilot = resolve(Controller::Copter, Autopilot::ArduPilot, &has).unwrap();
        assert_eq!(ardupilot.names().collect::<Vec<_>>(), ["ATC_RAT_RLL_P"]);
    }
}

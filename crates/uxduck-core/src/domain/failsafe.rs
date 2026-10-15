//! What a vehicle does when it loses its ground station (S81, ADR-0003
//! C16), from its own parameters, in one sentence: "If you lose connection
//! and nobody is at the ground station, Copter 1 will return to launch about
//! 15 s later." When nobody supervises a vehicle, UXDuck stops its ground
//! station heartbeat and this is what happens; UXDuck invents no recovery of
//! its own.
//!
//! Both firmwares ship with the failsafe off. ArduPilot counts only
//! heartbeats from its `SYSID_MYGCS`: 255 by default, as UXDuck's own
//! ground station id is unless changed in its settings.

use crate::domain::modes::Family;

/// What a vehicle would do, and whether it may be left with nobody at the
/// computer.
#[derive(Clone, Debug, PartialEq)]
pub struct FailsafePlan {
    /// "If you lose connection and nobody is at the ground station, …"
    pub sentence: String,
    /// Why it may not be left unattended, when it may not, with what to set.
    pub problem: Option<String>,
}

impl FailsafePlan {
    #[must_use]
    pub fn unattended_ok(&self) -> bool {
        self.problem.is_none()
    }
}

/// ArduPilot Copter's `FS_GCS_ENABLE`.
fn copter_action(value: f64) -> Option<&'static str> {
    #[allow(clippy::cast_possible_truncation, reason = "an enumeration")]
    match value.round() as i64 {
        1 | 2 => Some("return to launch"),
        3 => Some("return along its path, or to launch"),
        4 => Some("return along its path, or land"),
        5 => Some("land"),
        6 => Some("start its landing sequence, or return to launch"),
        7 => Some("brake, or land"),
        _ => None,
    }
}

/// ArduPilot Plane's `FS_LONG_ACTN`, once `FS_GCS_ENABL` is on.
fn plane_action(value: f64) -> &'static str {
    #[allow(clippy::cast_possible_truncation, reason = "an enumeration")]
    match value.round() as i64 {
        2 => "glide",
        3 => "deploy its parachute",
        4 => "start its landing sequence",
        _ => "return to launch",
    }
}

/// ArduPilot Rover's `FS_ACTION`, once `FS_GCS_ENABLE` is on.
fn rover_action(value: f64) -> Option<&'static str> {
    #[allow(clippy::cast_possible_truncation, reason = "an enumeration")]
    match value.round() as i64 {
        1 => Some("return to launch"),
        2 => Some("hold"),
        3 => Some("return along its path, or to launch"),
        4 => Some("return along its path, or hold"),
        5 => Some("stop"),
        _ => None,
    }
}

/// ArduPilot Sub's `FS_GCS_ENABLE`.
fn sub_action(value: f64) -> Option<&'static str> {
    #[allow(clippy::cast_possible_truncation, reason = "an enumeration")]
    match value.round() as i64 {
        2 => Some("disarm"),
        3 => Some("hold its depth"),
        4 => Some("surface"),
        _ => None,
    }
}

/// PX4's `NAV_DLL_ACT`.
fn px4_action(value: f64) -> Option<&'static str> {
    #[allow(clippy::cast_possible_truncation, reason = "an enumeration")]
    match value.round() as i64 {
        1 => Some("hold"),
        2 => Some("return to launch"),
        3 => Some("land"),
        5 => Some("terminate its flight"),
        6 => Some("lock down"),
        _ => None,
    }
}

fn seconds(s: f64) -> String {
    format!("{:.0} s", s.max(0.0))
}

/// What `name` does if it loses connection and nobody is at the ground
/// station, `grace_s` after UXDuck stops its heartbeat, by its parameters
/// (`param` reads one by name). `gcs_system_id` is the id UXDuck sends as.
/// Unknown parameters are taken as their firmware defaults, which are off.
#[must_use]
pub fn describe(
    family: Option<Family>,
    name: &str,
    grace_s: u32,
    gcs_system_id: u8,
    param: impl Fn(&str) -> Option<f64>,
) -> FailsafePlan {
    let grace = f64::from(grace_s);
    let lead = "If you lose connection and nobody is at the ground station, ";
    let off = |setting: &str| FailsafePlan {
        sentence: format!(
            "{lead}{name} will keep doing what it's doing, because its failsafe for losing the ground station is off."
        ),
        problem: Some(format!(
            "{name} has no failsafe for losing its ground station. Set {setting} to choose what it does"
        )),
    };
    let plan = |action: &str, timeout_s: f64| FailsafePlan {
        sentence: format!(
            "{lead}{name} will {action} about {} later.",
            seconds(grace + timeout_s)
        ),
        problem: None,
    };
    let Some(family) = family else {
        return FailsafePlan {
            sentence: format!(
                "{lead}UXDuck can't tell what {name} will do, because it can't read its failsafe settings."
            ),
            problem: Some(format!(
                "UXDuck cannot read {name}'s lost ground station failsafe"
            )),
        };
    };
    let mut result = match family {
        Family::Px4 => match px4_action(param("NAV_DLL_ACT").unwrap_or(0.0)) {
            Some(action) => plan(action, param("COM_DL_LOSS_T").unwrap_or(10.0)),
            None => off("NAV_DLL_ACT"),
        },
        Family::Copter => match copter_action(param("FS_GCS_ENABLE").unwrap_or(0.0)) {
            Some(action) => plan(action, param("FS_GCS_TIMEOUT").unwrap_or(5.0)),
            None => off("FS_GCS_ENABLE"),
        },
        Family::Plane => {
            if param("FS_GCS_ENABL").unwrap_or(0.0) >= 1.0 {
                plan(
                    plane_action(param("FS_LONG_ACTN").unwrap_or(0.0)),
                    param("FS_LONG_TIMEOUT").unwrap_or(5.0),
                )
            } else {
                off("FS_GCS_ENABL")
            }
        }
        Family::Rover => {
            let action = rover_action(param("FS_ACTION").unwrap_or(1.0));
            match (param("FS_GCS_ENABLE").unwrap_or(0.0) >= 1.0, action) {
                (true, Some(action)) => plan(action, param("FS_TIMEOUT").unwrap_or(1.5)),
                _ => off("FS_GCS_ENABLE and FS_ACTION"),
            }
        }
        // Blimp/events.cpp disarms on GCS loss in the pinned simulator.
        Family::Blimp => {
            if param("FS_GCS_ENABLE").unwrap_or(0.0) > 0.0 {
                plan("disarm", param("FS_GCS_TIMEOUT").unwrap_or(5.0))
            } else {
                off("FS_GCS_ENABLE")
            }
        }
        Family::Sub => match sub_action(param("FS_GCS_ENABLE").unwrap_or(0.0)) {
            Some(action) => plan(action, param("FS_GCS_TIMEOUT").unwrap_or(5.0)),
            None => off("FS_GCS_ENABLE"),
        },
    };
    // ArduPilot listens only to its own ground station.
    if family != Family::Px4
        && result.problem.is_none()
        && param("SYSID_MYGCS").is_some_and(|id| (id - f64::from(gcs_system_id)).abs() > 0.5)
    {
        result.problem = Some(format!(
            "{name} follows a different ground station, so it won't notice if UXDuck stops. Set SYSID_MYGCS to {gcs_system_id}"
        ));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, f64)]) -> impl Fn(&str) -> Option<f64> {
        let owned: Vec<(String, f64)> = pairs.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect();
        move |name| owned.iter().find(|(k, _)| k == name).map(|(_, v)| *v)
    }

    #[test]
    fn a_copter_set_to_return_says_so_with_the_time() {
        let plan = describe(
            Some(Family::Copter),
            "Copter 1",
            10,
            255,
            params(&[
                ("FS_GCS_ENABLE", 1.0),
                ("FS_GCS_TIMEOUT", 5.0),
                ("SYSID_MYGCS", 255.0),
            ]),
        );
        assert_eq!(
            plan.sentence,
            "If you lose connection and nobody is at the ground station, Copter 1 will return to launch about 15 s later."
        );
        assert!(plan.unattended_ok());
    }

    #[test]
    fn off_as_it_ships_it_may_not_be_left() {
        let plan = describe(Some(Family::Copter), "Copter 1", 10, 255, params(&[]));
        assert!(
            plan.sentence
                .ends_with("because its failsafe for losing the ground station is off.")
        );
        assert!(plan.problem.unwrap().contains("Set FS_GCS_ENABLE"));
    }

    #[test]
    fn another_ground_station_id_would_not_notice() {
        let plan = describe(
            Some(Family::Copter),
            "Copter 1",
            10,
            255,
            params(&[("FS_GCS_ENABLE", 5.0), ("SYSID_MYGCS", 254.0)]),
        );
        assert!(plan.sentence.contains("will land about 15 s later"));
        assert!(plan.problem.unwrap().contains("SYSID_MYGCS"));
    }

    #[test]
    fn a_ground_station_id_set_to_the_vehicles_is_noticed() {
        let plan = describe(
            Some(Family::Copter),
            "Copter 1",
            10,
            254,
            params(&[("FS_GCS_ENABLE", 5.0), ("SYSID_MYGCS", 254.0)]),
        );
        assert!(plan.unattended_ok(), "{:?}", plan.problem);
        let plan = describe(
            Some(Family::Copter),
            "Copter 1",
            10,
            254,
            params(&[("FS_GCS_ENABLE", 5.0), ("SYSID_MYGCS", 255.0)]),
        );
        assert!(plan.problem.unwrap().contains("Set SYSID_MYGCS to 254"));
    }

    #[test]
    fn each_firmware_reads_its_own_settings() {
        let plane = describe(
            Some(Family::Plane),
            "Plane 1",
            10,
            255,
            params(&[
                ("FS_GCS_ENABL", 1.0),
                ("FS_LONG_ACTN", 2.0),
                ("FS_LONG_TIMEOUT", 20.0),
            ]),
        );
        assert!(
            plane
                .sentence
                .contains("Plane 1 will glide about 30 s later"),
            "{}",
            plane.sentence
        );
        let rover = describe(
            Some(Family::Rover),
            "Rover 1",
            10,
            255,
            params(&[("FS_GCS_ENABLE", 1.0), ("FS_ACTION", 2.0)]),
        );
        assert!(
            rover.sentence.contains("will hold about 12 s later"),
            "{}",
            rover.sentence
        );
        let sub = describe(
            Some(Family::Sub),
            "Sub 1",
            10,
            255,
            params(&[("FS_GCS_ENABLE", 4.0)]),
        );
        assert!(sub.sentence.contains("will surface about 15 s later"));
        let px4 = describe(
            Some(Family::Px4),
            "PX4 1",
            10,
            255,
            params(&[("NAV_DLL_ACT", 2.0)]),
        );
        assert!(
            px4.sentence
                .contains("will return to launch about 20 s later")
        );
        assert!(px4.unattended_ok());
        assert!(!describe(Some(Family::Px4), "PX4 1", 10, 255, params(&[])).unattended_ok());
        assert!(!describe(None, "Thing", 10, 255, params(&[])).unattended_ok());
    }
    #[test]
    fn blimp_gcs_loss_disarms_in_the_pinned_firmware() {
        let plan = describe(
            Some(Family::Blimp),
            "Blimp 1",
            0,
            255,
            params(&[("FS_GCS_ENABLE", 5.0), ("FS_GCS_TIMEOUT", 7.0)]),
        );
        assert!(plan.sentence.contains("disarm"));
        assert!(plan.sentence.contains("7 s"));
        assert!(!plan.sentence.contains("land"));
    }
}

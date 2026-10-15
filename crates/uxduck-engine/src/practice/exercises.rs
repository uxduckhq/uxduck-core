//! The exercises (ADR-0024 "Exercises"), shipped in the binary as data
//! (X2), in the order the tab lists them: the first set, each with its
//! version for a VTOL plane where that flies it differently (S117), then
//! those for someone who knows the basics; then the guides, one for each
//! kind of simulated vehicle.

use std::sync::OnceLock;

use uxduck_core::domain::practice::Exercise;

const SOURCES: [&str; 22] = [
    include_str!("exercises/read-the-panel.json"),
    include_str!("exercises/read-the-panel-vtol.json"),
    include_str!("exercises/bring-it-home.json"),
    include_str!("exercises/bring-it-home-vtol.json"),
    include_str!("exercises/mode-tour.json"),
    include_str!("exercises/mode-tour-vtol.json"),
    include_str!("exercises/fly-there.json"),
    include_str!("exercises/fly-there-vtol.json"),
    include_str!("exercises/land-in-the-circle.json"),
    include_str!("exercises/land-in-the-circle-vtol.json"),
    include_str!("exercises/plan-the-survey.json"),
    include_str!("exercises/timed-delivery.json"),
    include_str!("exercises/hold-the-box.json"),
    include_str!("exercises/through-the-gates.json"),
    include_str!("exercises/parameter-puzzle.json"),
    // The guides (S117): one for each kind of simulated vehicle.
    include_str!("exercises/guide-multicopter.json"),
    include_str!("exercises/guide-vtol.json"),
    include_str!("exercises/guide-plane.json"),
    include_str!("exercises/guide-rover.json"),
    include_str!("exercises/guide-boat.json"),
    include_str!("exercises/guide-submarine.json"),
    include_str!("exercises/guide-airship.json"),
];

/// Every exercise, parsed once.
///
/// # Panics
/// A shipped exercise that does not parse, which the tests catch.
#[must_use]
pub fn all() -> &'static [Exercise] {
    static ALL: OnceLock<Vec<Exercise>> = OnceLock::new();
    ALL.get_or_init(|| {
        SOURCES
            .iter()
            .map(|source| {
                #[allow(clippy::expect_used, reason = "shipped data, checked by the tests")]
                serde_json::from_str(source).expect("a shipped exercise parses")
            })
            .collect()
    })
}

#[must_use]
pub fn find(id: &str) -> Option<&'static Exercise> {
    all().iter().find(|e| e.id == id)
}

#[cfg(test)]
mod tests {
    use uxduck_core::domain::practice::Check;

    use super::*;

    #[test]
    fn every_shipped_exercise_parses_and_names_only_its_own_features_and_questions() {
        let ids: Vec<&str> = all().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "read-the-panel",
                "read-the-panel-vtol",
                "bring-it-home",
                "bring-it-home-vtol",
                "mode-tour",
                "mode-tour-vtol",
                "fly-there",
                "fly-there-vtol",
                "land-in-the-circle",
                "land-in-the-circle-vtol",
                "plan-the-survey",
                "timed-delivery",
                "hold-the-box",
                "through-the-gates",
                "parameter-puzzle",
                "guide-multicopter",
                "guide-vtol",
                "guide-plane",
                "guide-rover",
                "guide-boat",
                "guide-submarine",
                "guide-airship"
            ]
        );
        for exercise in all() {
            assert!(!exercise.steps.is_empty(), "{}", exercise.id);
            assert!(
                !exercise.vehicles.is_empty(),
                "{} fits no vehicle",
                exercise.id
            );
            let checks = exercise
                .steps
                .iter()
                .flat_map(|s| &s.all)
                .chain(exercise.fails.iter().flat_map(|r| &r.all));
            for check in checks {
                match check {
                    Check::Within { feature, .. }
                    | Check::Outside { feature, .. }
                    | Check::GateCrossed { feature, .. }
                    | Check::MissionOutside { feature, .. }
                    | Check::MissionCovers { feature, .. } => assert!(
                        exercise.features.iter().any(|f| &f.id == feature),
                        "{}: no feature {feature}",
                        exercise.id
                    ),
                    Check::Answered { question } => assert!(
                        exercise.questions.iter().any(|q| &q.id == question),
                        "{}: no question {question}",
                        exercise.id
                    ),
                    _ => {}
                }
            }
        }
    }

    /// A version of an exercise for another vehicle keeps its title, so the
    /// tab, listing only those that fit, shows the same names.
    #[test]
    fn versions_for_other_vehicles_keep_their_titles_and_fit_no_vehicle_twice() {
        for exercise in all().iter().filter(|e| !e.guide) {
            let same: Vec<&Exercise> = all()
                .iter()
                .filter(|e| !e.guide && e.title == exercise.title)
                .collect();
            for vehicle in &exercise.vehicles {
                let fitting = same.iter().filter(|e| e.vehicles.contains(vehicle)).count();
                assert_eq!(fitting, 1, "{} twice for {vehicle:?}", exercise.title);
            }
        }
    }

    /// A guide (S117): one per kind of simulated vehicle, called Get
    /// started, for a first flight, judged on the vehicle as it is, never
    /// failed and without a measure; 4 to 6 steps, each with its hint.
    #[test]
    fn there_is_one_guide_for_every_simulated_vehicle_and_it_cannot_fail() {
        use uxduck_core::domain::link::SimVehicle;
        use uxduck_core::domain::practice::{Audience, ExerciseStart};
        for kind in [
            SimVehicle::Multicopter,
            SimVehicle::Vtol,
            SimVehicle::Plane,
            SimVehicle::Rover,
            SimVehicle::Boat,
            SimVehicle::Submarine,
            SimVehicle::Airship,
        ] {
            let guides: Vec<&Exercise> = all()
                .iter()
                .filter(|e| e.guide && e.vehicles.contains(&kind))
                .collect();
            assert_eq!(guides.len(), 1, "{kind:?}");
            let guide = guides[0];
            let name = serde_json::to_value(kind).unwrap();
            assert_eq!(guide.id, format!("guide-{}", name.as_str().unwrap()));
            assert_eq!(guide.vehicles, [kind]);
            assert_eq!(guide.title, "Get started");
            assert_eq!(guide.audience, Audience::FirstFlight);
            assert!(guide.fails.is_empty(), "{}", guide.id);
            assert!(guide.measure.is_none(), "{}", guide.id);
            assert!(guide.questions.is_empty(), "{}", guide.id);
            assert!(!guide.needs_controller, "{}", guide.id);
            assert_eq!(
                guide.start,
                ExerciseStart {
                    height_m: 0.0,
                    on_ground: false,
                    offset: None,
                    battery_pct: None,
                    parameters: Vec::new(),
                },
                "{}: a guide starts on the vehicle as it is",
                guide.id
            );
            assert!((4..=6).contains(&guide.steps.len()), "{}", guide.id);
            for step in &guide.steps {
                assert!(
                    step.hint.is_some() && step.hint_after_s.is_some(),
                    "{}",
                    step.text
                );
                // Judged only on what the vehicle and the person did.
                for check in &step.all {
                    assert!(
                        matches!(
                            check,
                            Check::CommandSent { .. }
                                | Check::Mode { .. }
                                | Check::Within { .. }
                                | Check::Landed
                                | Check::Airborne
                                | Check::Armed
                                | Check::HeightBetween { .. }
                        ),
                        "{}: {check:?}",
                        guide.id
                    );
                }
            }
        }
    }
}

//! AutoTune, guided (S332, ADR-0054 T10). A driver per controller: the
//! mode a person flies it in, the parameters that choose its axes and how
//! hard it pushes, the transmitter switch that turns it on, and what the
//! firmware does with the gains it finds. Its status texts are read into
//! the axis and stage it is at, and folded into a run.

use serde::{Deserialize, Serialize};

use super::tuning::{Axis, Controller};

/// How AutoTune works on a controller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Driver {
    pub controller: Controller,
    /// The mode's name, as `command/send`'s `set-mode` takes it.
    pub mode: &'static str,
    /// Its number, as the mode switch's `FLTMODEn` holds it.
    pub mode_number: u32,
    /// The bitmask of axes to tune.
    pub axes: &'static str,
    /// How hard it pushes: `AUTOTUNE_AGGR` or a plane's `AUTOTUNE_LEVEL`.
    pub strength: &'static str,
    /// The `RCx_OPTION` that turns it on from a switch, where there is one.
    pub switch_option: Option<u32>,
    /// What the firmware does with the gains it found, in words.
    pub keeps: &'static str,
    /// What it saves beyond the Tuning tab's own parameters, each one's
    /// names newest first: `AC_AutoTune_Multi::save_tuning_gains` saves each
    /// axis's acceleration limit and turns feedforward on, and
    /// `AP_AutoTune::save_gains` a plane's most rate.
    pub saves: &'static [&'static [&'static str]],
}

impl Driver {
    /// The parameters it saves beyond the Tuning tab's, by the names the
    /// vehicle has.
    pub fn saved(&self, has: impl Fn(&str) -> bool) -> impl Iterator<Item = &'static str> {
        self.saves
            .iter()
            .filter_map(move |names| names.iter().copied().find(|n| has(n)))
    }
}

/// The AutoTune of a controller.
#[must_use]
pub const fn driver(controller: Controller) -> Driver {
    match controller {
        Controller::Copter => Driver {
            controller,
            mode: "AUTOTUNE",
            mode_number: 15,
            axes: "AUTOTUNE_AXES",
            strength: "AUTOTUNE_AGGR",
            switch_option: Some(17),
            keeps: "It keeps the new gains when it lands and disarms in AutoTune. Switch out of AutoTune before that and it flies on the gains from before.",
            saves: &[
                &["ATC_ACC_R_MAX", "ATC_ACCEL_R_MAX"],
                &["ATC_ACC_P_MAX", "ATC_ACCEL_P_MAX"],
                &["ATC_ACC_Y_MAX", "ATC_ACCEL_Y_MAX"],
                &["ATC_RATE_FF_ENAB"],
            ],
        },
        Controller::Hover => Driver {
            controller,
            mode: "QAUTOTUNE",
            mode_number: 22,
            axes: "Q_AUTOTUNE_AXES",
            strength: "Q_AUTOTUNE_AGGR",
            switch_option: None,
            keeps: "It keeps the new gains when it lands and disarms in QAUTOTUNE. Switch out of QAUTOTUNE before that and it flies on the gains from before.",
            saves: &[
                &["Q_A_ACC_R_MAX", "Q_A_ACCEL_R_MAX"],
                &["Q_A_ACC_P_MAX", "Q_A_ACCEL_P_MAX"],
                &["Q_A_ACC_Y_MAX", "Q_A_ACCEL_Y_MAX"],
                &["Q_A_RATE_FF_ENAB"],
            ],
        },
        Controller::Plane => Driver {
            controller,
            mode: "AUTOTUNE",
            mode_number: 8,
            axes: "AUTOTUNE_AXES",
            strength: "AUTOTUNE_LEVEL",
            switch_option: Some(107),
            keeps: "It keeps an axis's new gains once that axis has finished. An axis that has not finished goes back to its gains from before when it leaves AUTOTUNE.",
            saves: &[
                &["RLL2SRV_RMAX"],
                &["PTCH2SRV_RMAX_UP"],
                &["PTCH2SRV_RMAX_DN"],
            ],
        },
    }
}

/// How AutoTune is entered from the transmitter now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    /// The mode switch's channel, when it has one.
    pub channel: Option<u32>,
    /// The mode switch's positions, 1 to 6, that are AutoTune.
    pub positions: Vec<u32>,
    /// The channels whose switch turns AutoTune on.
    pub switches: Vec<u32>,
}

/// Where the vehicle's transmitter enters AutoTune, from its parameters.
#[must_use]
pub fn entry(driver: &Driver, value: impl Fn(&str) -> Option<f64>) -> Entry {
    let is = |name: &str, want: u32| value(name).is_some_and(|v| (v - f64::from(want)).abs() < 0.5);
    let channel = (1..=16).find(|n| is("FLTMODE_CH", *n));
    let positions = (1..=6)
        .filter(|n| is(&format!("FLTMODE{n}"), driver.mode_number))
        .collect();
    let switches = driver.switch_option.map_or_else(Vec::new, |option| {
        (1..=16)
            .filter(|n| is(&format!("RC{n}_OPTION"), option))
            .collect()
    });
    Entry {
        channel,
        positions,
        switches,
    }
}

/// Which AutoTune a status text came from: a copter's or a hover's
/// (`AutoTune: `), or a plane's fixed-wing one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Multi,
    Fixed,
}

/// What one status text said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Said {
    Started,
    /// An axis and the stage of it, with how far through the stage.
    Stage {
        axis: Axis,
        stage: String,
        percent: Option<u8>,
    },
    /// Between twitches, waiting for it to level, or a twitch.
    Twitching,
    /// The person is moving the sticks: it waits.
    Paused,
    /// An axis has its gains.
    AxisDone(Axis),
    /// Every axis is done: it is flying on the new gains.
    Success,
    /// The gains are written to its storage.
    Saved,
    /// Back in AutoTune after it was done, trying the new gains.
    Trying,
    /// Back on the gains from before.
    Restored,
    Failed(String),
    /// It left the mode.
    Stopped,
    /// Worth showing, changing nothing: a limit it reached.
    Note(String),
}

fn axis_of(word: &str) -> Option<Axis> {
    match word {
        "Roll" => Some(Axis::Roll),
        "Pitch" => Some(Axis::Pitch),
        "Yaw" | "Yaw(E)" | "Yaw(D)" => Some(Axis::Yaw),
        _ => None,
    }
}

/// Reads a status text from AutoTune, as `ArduCopter`'s `AC_AutoTune` and
/// `ArduPlane`'s `AP_AutoTune` word them; anything else is `None`.
#[must_use]
pub fn said(text: &str) -> Option<(Family, Said)> {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix("AutoTune: ") {
        return Some((Family::Multi, multi(rest)?));
    }
    fixed(text).map(|s| (Family::Fixed, s))
}

fn multi(rest: &str) -> Option<Said> {
    let said = match rest {
        "Started" => Said::Started,
        "Stopped" => Said::Stopped,
        "Success" => Said::Success,
        "Leveling" | "Testing" | "Updating Gains" => Said::Twitching,
        "Paused: Pilot Override Active" | "pilot overrides active" => Said::Paused,
        "original gains restored" => Said::Restored,
        "Aborting Test" => Said::Note("A twitch went too far: it tries again".into()),
        _ if rest.starts_with("Saved gains for") => Said::Saved,
        _ if rest.starts_with("Pilot Testing gains for") => Said::Trying,
        _ if rest.contains("Failed") => Said::Failed(rest.to_owned()),
        _ if rest.contains("limit reached") => Said::Note(rest.to_owned()),
        _ => {
            let (axis, tail) = rest.split_once(' ')?;
            let axis = axis_of(axis)?;
            if tail == "complete" {
                return Some(Said::AxisDone(axis));
            }
            // "Rate: P:0.135, ..." and "Angle P:4.5, ..." report gains,
            // which are read from the parameters once it saves them.
            if tail.contains(':') {
                return None;
            }
            let (stage, percent) = match tail.rsplit_once(' ') {
                Some((stage, last)) if last.ends_with('%') => {
                    (stage, last.trim_end_matches('%').parse::<u8>().ok())
                }
                _ => (tail, None),
            };
            Said::Stage {
                axis,
                stage: stage_words(stage),
                percent,
            }
        }
    };
    Some(said)
}

fn fixed(text: &str) -> Option<Said> {
    if text == "Started autotune" || text.starts_with("Autotuning ") {
        return Some(Said::Started);
    }
    if text == "Stopped autotune" {
        return Some(Said::Stopped);
    }
    if let Some(axis) = text.strip_suffix(": Finished") {
        return axis_of(axis).map(Said::AxisDone);
    }
    // "RollD: 0.0042", "PitchP: 1.2": the limit it found for that gain.
    let (head, number) = text.split_once(": ")?;
    number.parse::<f64>().ok()?;
    let (axis, gain) = head.split_at(head.len().checked_sub(1)?);
    let stage = match gain {
        "D" => "found the highest D",
        "P" => "found the highest P",
        _ => return None,
    };
    Some(Said::Stage {
        axis: axis_of(axis)?,
        stage: stage.into(),
        percent: None,
    })
}

/// A stage as a person reads it.
fn stage_words(stage: &str) -> String {
    match stage {
        "Rate D Up" => "raising rate D",
        "Rate D Down" => "lowering rate D",
        "Rate P Up" => "raising rate P",
        "Rate FF Up" => "raising feed-forward",
        "Angle P Up" => "raising angle P",
        "Angle P Down" => "lowering angle P",
        "Find Max Gains" => "finding the highest gains",
        "Check Tune Frequency Response" => "checking the tune",
        "Tune Complete" => "done",
        other => return other.to_lowercase(),
    }
    .to_owned()
}

/// Where a run is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Running,
    /// Every axis it was asked for is done, not kept yet.
    Done,
    /// The firmware kept the new gains.
    Kept,
    /// It left AutoTune with the gains from before.
    Stopped,
    Failed,
}

/// One AutoTune, from its start to its end, folded from what it said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub controller: Controller,
    pub phase: Phase,
    pub axis: Option<Axis>,
    pub stage: Option<String>,
    pub percent: Option<u8>,
    pub paused: bool,
    /// Done, then switched out of the mode: it flies on the gains from
    /// before until it is back in AutoTune to try or keep the new ones.
    pub away: bool,
    /// The axes that have their gains, in the order they finished.
    pub done: Vec<Axis>,
    /// A failure's reason or a limit it reached.
    pub note: Option<String>,
}

impl Run {
    #[must_use]
    pub const fn new(controller: Controller) -> Self {
        Self {
            controller,
            phase: Phase::Running,
            axis: None,
            stage: None,
            percent: None,
            paused: false,
            away: false,
            done: Vec::new(),
            note: None,
        }
    }

    /// Whether it has ended, kept or not.
    #[must_use]
    pub const fn ended(&self) -> bool {
        matches!(self.phase, Phase::Kept | Phase::Stopped | Phase::Failed)
    }

    /// Takes in what it said.
    pub fn said(&mut self, said: Said) {
        self.paused = matches!(said, Said::Paused);
        match said {
            Said::Trying => self.away = false,
            Said::Started | Said::Twitching | Said::Paused => {}
            Said::Stage {
                axis,
                stage,
                percent,
            } => {
                self.axis = Some(axis);
                self.stage = Some(stage);
                self.percent = percent;
                if self.phase == Phase::Done {
                    self.phase = Phase::Running;
                }
            }
            Said::AxisDone(axis) => {
                if !self.done.contains(&axis) {
                    self.done.push(axis);
                }
                self.stage = None;
                self.percent = None;
            }
            Said::Success => self.phase = Phase::Done,
            Said::Saved => self.phase = Phase::Kept,
            Said::Restored => {
                if self.phase != Phase::Kept {
                    self.phase = Phase::Done;
                }
            }
            Said::Failed(why) => {
                self.phase = Phase::Failed;
                self.note = Some(why);
            }
            Said::Note(note) => self.note = Some(note),
            Said::Stopped => {
                self.phase = match (self.phase, self.controller) {
                    (Phase::Kept | Phase::Failed, _) => self.phase,
                    // A plane keeps each axis that finished.
                    (_, Controller::Plane) if !self.done.is_empty() => Phase::Kept,
                    // A copter keeps the new gains in memory to try again.
                    (Phase::Done, _) => {
                        self.away = true;
                        Phase::Done
                    }
                    _ => Phase::Stopped,
                };
            }
        }
    }

    /// The one line to read at arm's length.
    #[must_use]
    pub fn line(&self) -> String {
        let done = axes_words(&self.done);
        match self.phase {
            Phase::Kept => format!("Done: it kept the new gains for {done}"),
            Phase::Stopped => {
                "Stopped before it was kept: it flies on the gains from before".into()
            }
            Phase::Failed => format!(
                "AutoTune failed: {}",
                self.note.as_deref().unwrap_or("it gave no reason")
            ),
            Phase::Done if self.away => format!(
                "{done} done, flying on the gains from before: switch back to {} to try the new ones, and land and disarm in it to keep them",
                driver(self.controller).mode
            ),
            Phase::Done => match self.controller {
                Controller::Plane => format!("{done} done: they are kept"),
                Controller::Hover => {
                    format!("{done} done: land and disarm in QAUTOTUNE to keep the new gains")
                }
                Controller::Copter => {
                    format!("{done} done: land and disarm in AutoTune to keep the new gains")
                }
            },
            Phase::Running if self.paused => "Paused while you move the sticks".into(),
            Phase::Running => match (self.axis, &self.stage) {
                (Some(axis), Some(stage)) => match self.percent {
                    Some(percent) => format!("{}: {stage}, {percent}%", axis_word(axis)),
                    None => format!("{}: {stage}", axis_word(axis)),
                },
                _ => "Started: it twitches, then levels, again and again".into(),
            },
        }
    }
}

const fn axis_word(axis: Axis) -> &'static str {
    match axis {
        Axis::Roll => "Roll",
        Axis::Pitch => "Pitch",
        Axis::Yaw => "Yaw",
    }
}

/// Axes in words: "Roll and Pitch".
#[must_use]
pub fn axes_words(axes: &[Axis]) -> String {
    let words: Vec<&str> = axes.iter().map(|a| axis_word(*a)).collect();
    match words.as_slice() {
        [] => "no axis".into(),
        [one] => (*one).into(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A copter's AutoTune of roll and pitch, as `ArduCopter` 4.6 words it.
    const COPTER: [&str; 14] = [
        "AutoTune: Started",
        "AutoTune: Roll Rate D Up 0%",
        "AutoTune: Leveling",
        "AutoTune: Roll Rate D Up 50%",
        "AutoTune: Paused: Pilot Override Active",
        "AutoTune: Roll Angle P Up 75%",
        "AutoTune: Roll complete",
        "AutoTune: Roll Rate: P:0.172, I:0.172, D:0.0061",
        "AutoTune: Roll Angle P:8.123, Max Accel:120000",
        "AutoTune: Pitch Rate P Up 25%",
        "AutoTune: Pitch complete",
        "AutoTune: Success",
        "AutoTune: Saved gains for Roll Pitch ",
        "AutoTune: Stopped",
    ];

    fn fold(controller: Controller, texts: &[&str]) -> (Run, Vec<String>) {
        let mut run = Run::new(controller);
        let mut lines = Vec::new();
        for text in texts {
            if let Some((_, s)) = said(text) {
                run.said(s);
                lines.push(run.line());
            }
        }
        (run, lines)
    }

    #[test]
    fn reads_a_copters_autotune_into_axis_stage_and_its_end() {
        let (run, lines) = fold(Controller::Copter, &COPTER);
        assert_eq!(lines[1], "Roll: raising rate D, 0%");
        assert_eq!(lines[3], "Roll: raising rate D, 50%");
        assert_eq!(lines[4], "Paused while you move the sticks");
        assert_eq!(lines[5], "Roll: raising angle P, 75%");
        assert!(lines.contains(&"Pitch: raising rate P, 25%".to_owned()));
        assert!(lines.contains(
            &"Roll and Pitch done: land and disarm in AutoTune to keep the new gains".to_owned()
        ));
        assert_eq!(run.phase, Phase::Kept);
        assert_eq!(run.done, [Axis::Roll, Axis::Pitch]);
        assert_eq!(run.line(), "Done: it kept the new gains for Roll and Pitch");
    }

    #[test]
    fn a_copter_switched_out_before_landing_keeps_nothing() {
        let (run, _) = fold(
            Controller::Copter,
            &[
                "AutoTune: Started",
                "AutoTune: Roll Rate D Up 25%",
                "AutoTune: Stopped",
            ],
        );
        assert_eq!(run.phase, Phase::Stopped);
        let mut away = fold(
            Controller::Copter,
            &[
                "AutoTune: Started",
                "AutoTune: Roll complete",
                "AutoTune: Success",
                "AutoTune: Stopped",
            ],
        )
        .0;
        assert_eq!((away.phase, away.away), (Phase::Done, true));
        assert!(
            away.line()
                .starts_with("Roll done, flying on the gains from before")
        );
        away.said(Said::Trying);
        away.said(Said::Saved);
        assert_eq!(away.phase, Phase::Kept);
        let (failed, _) = fold(
            Controller::Copter,
            &[
                "AutoTune: Started",
                "AutoTune: Failed to level, please tune manually",
                "AutoTune: Stopped",
            ],
        );
        assert_eq!(failed.phase, Phase::Failed);
        assert_eq!(
            failed.line(),
            "AutoTune failed: Failed to level, please tune manually"
        );
    }

    #[test]
    fn reads_a_planes_autotune_and_keeps_the_axes_that_finished() {
        let texts = [
            "Started autotune",
            "Autotuning roll pitch ",
            "RollD: 0.0042",
            "RollP: 1.2000",
            "Roll: Finished",
            "PitchD: 0.0031",
            "Stopped autotune",
        ];
        assert_eq!(said("RollD: 0.0042").map(|s| s.0), Some(Family::Fixed));
        let (run, lines) = fold(Controller::Plane, &texts);
        assert_eq!(lines[2], "Roll: found the highest D");
        assert_eq!(run.phase, Phase::Kept);
        assert_eq!(run.done, [Axis::Roll]);
        let (none, _) = fold(Controller::Plane, &["Started autotune", "Stopped autotune"]);
        assert_eq!(none.phase, Phase::Stopped);
    }

    #[test]
    fn leaves_other_texts_and_gain_reports_alone() {
        for text in [
            "PreArm: Throttle below failsafe",
            "AutoTune: Roll Rate: P:0.1, I:0.1, D:0.004",
            "Battery: 50%",
            "Mode: AUTOTUNE",
        ] {
            assert_eq!(said(text), None, "{text}");
        }
        assert_eq!(
            said("AutoTune: Yaw(D) Rate D Up 25%").map(|s| s.1),
            Some(Said::Stage {
                axis: Axis::Yaw,
                stage: "raising rate D".into(),
                percent: Some(25)
            })
        );
    }

    #[test]
    fn names_what_it_saves_beyond_the_tuning_tab_by_the_vehicles_names() {
        let newer = |n: &str| n.starts_with("ATC_ACC_") || n == "ATC_RATE_FF_ENAB";
        assert_eq!(
            driver(Controller::Copter).saved(newer).collect::<Vec<_>>(),
            [
                "ATC_ACC_R_MAX",
                "ATC_ACC_P_MAX",
                "ATC_ACC_Y_MAX",
                "ATC_RATE_FF_ENAB"
            ]
        );
        let older = |n: &str| n.starts_with("Q_A_ACCEL_");
        assert_eq!(
            driver(Controller::Hover).saved(older).collect::<Vec<_>>(),
            ["Q_A_ACCEL_R_MAX", "Q_A_ACCEL_P_MAX", "Q_A_ACCEL_Y_MAX"]
        );
        assert_eq!(
            driver(Controller::Plane).saved(|_| true).count(),
            3,
            "its most rates"
        );
    }

    #[test]
    fn finds_where_the_transmitter_enters_autotune() {
        let copter = driver(Controller::Copter);
        let values = |name: &str| match name {
            "FLTMODE_CH" => Some(5.0),
            "FLTMODE6" => Some(15.0),
            "FLTMODE1" | "RC8_OPTION" => Some(0.0),
            "RC7_OPTION" => Some(17.0),
            _ => None,
        };
        assert_eq!(
            entry(&copter, values),
            Entry {
                channel: Some(5),
                positions: vec![6],
                switches: vec![7]
            }
        );
        assert!(
            entry(&driver(Controller::Plane), values)
                .positions
                .is_empty()
        );
        assert_eq!(entry(&copter, |_| None), Entry::default());
    }
}

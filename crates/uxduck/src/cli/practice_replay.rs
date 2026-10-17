//! `uxduck dev replay --practice <exercise>` (S100, ADR-0024 "Replay"): an
//! attempt's tlog judged again, here, without a daemon. The judge is a
//! function of the vehicle's state (X5), so what the vehicle sent and what
//! the ground station sent bring the same steps and the same result.

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use uxduck_core::domain::command::Command;
use uxduck_core::domain::modes::Family;
use uxduck_core::domain::practice::{Attempt, AttemptState, Exercise};
use uxduck_core::domain::telemetry::AltitudeFrame;
use uxduck_engine::practice::exercise;
use uxduck_engine::practice::judge::{self, Sample, Verdict, View};

/// The track keeps a point at most this often, as the engine does.
const SAMPLE_EVERY_US: u64 = 1_000_000;
/// A controller counts as driving it while its sticks were heard this recently.
const STICKS_FRESH_US: u64 = 2_000_000;
/// `MAV_AUTOPILOT_INVALID`: a ground station's heartbeat, not a vehicle's.
const NOT_AN_AUTOPILOT: i64 = 8;
/// `MAV_MODE_FLAG_SAFETY_ARMED`.
const ARMED: i64 = 128;
/// `MAV_CMD_NAV_WAYPOINT` and `MAV_CMD_NAV_SPLINE_WAYPOINT`.
const WAYPOINTS: [i64; 2] = [16, 82];

/// What a replay found: when each step began, and how it ended.
#[derive(Debug, PartialEq)]
pub struct Replayed {
    /// Seconds into the tlog, and the step from then on.
    pub steps: Vec<(f64, u32)>,
    /// Seconds into the tlog it ended, or the end of the tlog.
    pub at_s: f64,
    pub verdict: Verdict,
}

#[derive(Default)]
struct Replay {
    vehicle: Option<u8>,
    view: View,
    home: Option<(f64, f64)>,
    /// A mission being uploaded, by sequence: its command and place.
    uploading: BTreeMap<i64, (i64, (f64, f64))>,
    sticks_at: Option<u64>,
}

fn int(fields: &Map<String, Value>, key: &str) -> Option<i64> {
    fields.get(key).and_then(Value::as_i64)
}

fn float(fields: &Map<String, Value>, key: &str) -> Option<f64> {
    fields.get(key).and_then(Value::as_f64)
}

/// A MAVLink 32-bit field, such as a position in 1e-7 degrees or a height
/// in millimetres, as a float.
fn wide(fields: &Map<String, Value>, key: &str) -> Option<f64> {
    int(fields, key)
        .and_then(|v| i32::try_from(v).ok())
        .map(f64::from)
}

fn degrees(fields: &Map<String, Value>, key: &str) -> Option<f64> {
    wide(fields, key).map(|v| v / 1e7)
}

fn mode_name(custom_mode: Option<i64>) -> Option<String> {
    Family::Copter
        .name(u32::try_from(custom_mode?).ok()?)
        .map(str::to_owned)
}

/// A mode number sent as a command's float parameter.
fn mode_param(param: Option<f64>) -> Option<String> {
    let param = param.filter(|p| (0.0..=f64::from(u32::MAX)).contains(p))?;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "checked to be a u32 above"
    )]
    mode_name(Some(i64::from(param.round() as u32)))
}

impl Replay {
    /// What the vehicle said.
    fn heard(&mut self, name: &str, f: &Map<String, Value>) {
        match name {
            "HEARTBEAT" => {
                self.view.mode = mode_name(int(f, "custom_mode")).or(self.view.mode.take());
                self.view.armed = int(f, "base_mode").is_some_and(|m| m & ARMED != 0);
            }
            "GLOBAL_POSITION_INT" => {
                if let (Some(lat), Some(lon)) = (degrees(f, "lat"), degrees(f, "lon")) {
                    self.view.position = Some((lat, lon));
                }
                self.view.height_m = wide(f, "relative_alt").map(|mm| mm / 1000.0);
            }
            "SYS_STATUS" | "BATTERY_STATUS" => {
                if let Some(pct) = wide(f, "battery_remaining").filter(|p| *p >= 0.0) {
                    self.view.battery_pct = Some(pct);
                }
            }
            "EXTENDED_SYS_STATE" => {
                self.view.on_ground = match int(f, "landed_state") {
                    Some(1) => Some(true),
                    Some(2..=4) => Some(false),
                    _ => None,
                };
            }
            // The fence and the ground below (S132).
            "FENCE_STATUS" => {
                self.view.fence_breached = int(f, "breach_status").map(|b| b != 0);
            }
            "TERRAIN_REPORT" if int(f, "spacing").is_some_and(|s| s > 0) => {
                self.view.clearance_m = float(f, "current_height");
            }
            "HOME_POSITION" => {
                if let (Some(lat), Some(lon)) = (degrees(f, "latitude"), degrees(f, "longitude")) {
                    self.home = Some((lat, lon));
                }
            }
            // The upload accepted: its waypoints are the mission.
            "MISSION_ACK" if int(f, "type") == Some(0) && !self.uploading.is_empty() => {
                let places = self
                    .uploading
                    .iter()
                    .filter(|(seq, (command, _))| **seq > 0 && WAYPOINTS.contains(command))
                    .map(|(_, (_, at))| *at)
                    .collect();
                self.view.mission = Some(places);
                self.view.mission_commands = Some(
                    self.uploading
                        .iter()
                        .filter(|(seq, _)| **seq > 0)
                        .filter_map(|(_, (command, _))| u16::try_from(*command).ok())
                        .collect(),
                );
                self.uploading.clear();
            }
            _ => {}
        }
    }

    /// What the ground station sent it: the operator's commands,
    /// parameters, mission and sticks.
    fn sent(&mut self, at: u64, name: &str, f: &Map<String, Value>) {
        let command = match name {
            "COMMAND_LONG" | "COMMAND_INT" => match int(f, "command") {
                Some(400) if float(f, "param1") == Some(1.0) => Some(Command::Arm),
                Some(20) => Some(Command::Rtl),
                Some(21) => Some(Command::Land),
                Some(176) => mode_param(float(f, "param2")).map(|mode| Command::SetMode { mode }),
                Some(192) if name == "COMMAND_INT" => {
                    degrees(f, "x")
                        .zip(degrees(f, "y"))
                        .map(|(lat, lon)| Command::GoTo {
                            latitude_deg: lat,
                            longitude_deg: lon,
                            altitude_m: float(f, "z").unwrap_or_default(),
                            altitude_frame: AltitudeFrame::AboveHome,
                        })
                }
                _ => None,
            },
            "SET_MODE" => mode_name(int(f, "custom_mode")).map(|mode| Command::SetMode { mode }),
            "MISSION_COUNT" => {
                self.uploading.clear();
                None
            }
            "MISSION_ITEM_INT" => {
                let place = degrees(f, "x").zip(degrees(f, "y"));
                // Current 2: a place to go to now, as Guided's "fly here".
                if int(f, "current") == Some(2) {
                    place.map(|(lat, lon)| Command::GoTo {
                        latitude_deg: lat,
                        longitude_deg: lon,
                        altitude_m: float(f, "z").unwrap_or_default(),
                        altitude_frame: AltitudeFrame::AboveHome,
                    })
                } else {
                    if let (Some(seq), Some(command), Some(place)) =
                        (int(f, "seq"), int(f, "command"), place)
                    {
                        self.uploading.insert(seq, (command, place));
                    }
                    None
                }
            }
            "PARAM_SET" => {
                let id = f.get("param_id").and_then(Value::as_str);
                if let (Some(id), Some(value)) = (id, float(f, "param_value")) {
                    let id = id.trim_end_matches('\0').to_owned();
                    self.view.parameters_set.push((at, id, value));
                }
                None
            }
            "MANUAL_CONTROL" | "RC_CHANNELS_OVERRIDE" => {
                self.sticks_at = Some(at);
                None
            }
            _ => None,
        };
        if let Some(command) = command {
            self.view.commands.push((at, command));
        }
    }
}

/// The attempt as the engine has it once set up, at `home`, from `at`.
fn begun(exercise: &Exercise, home: (f64, f64), at: u64) -> Attempt {
    Attempt {
        id: "replay".into(),
        exercise_id: exercise.id.clone(),
        exercise_version: exercise.version,
        guide: exercise.guide,
        vehicle_id: "replay".into(),
        link_id: "replay".into(),
        operator_id: None,
        state: AttemptState::Running,
        step: 0,
        line: None,
        measure: None,
        home_latitude_deg: home.0,
        home_longitude_deg: home.1,
        features: judge::place_features(exercise, home),
        answered: std::collections::BTreeSet::new(),
        wrong: BTreeMap::new(),
        set_up: true,
        restore: Vec::new(),
        started_at_us: at,
        step_started_at_us: at,
        ended_at_us: None,
        version: 1,
    }
}

impl Replay {
    /// Adds where it is to the track, as often as the engine does.
    fn sample(&mut self, at: u64, position: (f64, f64)) {
        let due = self
            .view
            .track
            .last()
            .is_none_or(|s| at.saturating_sub(s.at_us) >= SAMPLE_EVERY_US);
        if due {
            self.view.track.push(Sample {
                at_us: at,
                position,
                height_m: self.view.height_m,
                mode: self.view.mode.clone(),
                clearance_m: self.view.clearance_m,
            });
        }
    }
}

/// Judges an exercise over a tlog's frames, from `from_s` seconds into it.
///
/// # Errors
/// An unknown exercise, or a tlog with no vehicle in it.
pub fn judge_tlog(bytes: &[u8], exercise_id: &str, from_s: f64) -> Result<Replayed, String> {
    let exercise = exercise(exercise_id).ok_or_else(|| format!("no exercise {exercise_id}"))?;
    let records = uxduck_mavlink::tlog::read(bytes);
    let first = records
        .first()
        .map(|(at, _)| *at)
        .ok_or("the tlog holds no frames")?;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "seconds"
    )]
    let judged_from = first + (from_s.max(0.0) * 1e6) as u64;
    #[allow(clippy::cast_precision_loss, reason = "microseconds of a flight")]
    let seconds = |at: u64| (at - first) as f64 / 1e6;
    let mut replay = Replay::default();
    let mut attempt: Option<Attempt> = None;
    let mut steps = Vec::new();
    let mut last = first;
    for (at, frame) in &records {
        let at = *at;
        last = at;
        let Some(header) = uxduck_mavlink::framing::header(frame) else {
            continue;
        };
        let Some(decoded) = uxduck_mavlink::framing::payload(frame)
            .and_then(|payload| uxduck_mavlink::named::decode(header.message_id, payload))
        else {
            continue;
        };
        let f = &decoded.fields;
        if replay.vehicle.is_none()
            && decoded.name == "HEARTBEAT"
            && int(f, "autopilot").is_some_and(|a| a != NOT_AN_AUTOPILOT)
        {
            replay.vehicle = Some(header.system_id);
        }
        if Some(header.system_id) == replay.vehicle {
            replay.heard(&decoded.name, f);
        } else {
            replay.sent(at, &decoded.name, f);
            continue;
        }
        // Judged as the engine does, on the vehicle's news of where it is.
        if decoded.name != "GLOBAL_POSITION_INT" || at < judged_from {
            continue;
        }
        let Some(position) = replay.view.position else {
            continue;
        };
        let attempt = attempt.get_or_insert_with(|| {
            steps.push((seconds(at), 0));
            begun(exercise, replay.home.unwrap_or(position), at)
        });
        replay.view.now_us = at;
        replay.view.controller = replay
            .sticks_at
            .is_some_and(|heard| at.saturating_sub(heard) <= STICKS_FRESH_US);
        replay.sample(at, position);
        match judge::judge(exercise, attempt, &replay.view) {
            Verdict::Keep => {}
            Verdict::Step(step) => {
                attempt.step = step;
                attempt.step_started_at_us = at;
                steps.push((seconds(at), step));
            }
            verdict => {
                return Ok(Replayed {
                    steps,
                    at_s: seconds(at),
                    verdict,
                });
            }
        }
    }
    if replay.vehicle.is_none() {
        return Err("no vehicle's heartbeat in the tlog".into());
    }
    Ok(Replayed {
        steps,
        at_s: seconds(last),
        verdict: Verdict::Keep,
    })
}

/// Prints a replay as `uxduck dev replay --practice` does.
pub fn print(exercise_id: &str, replayed: &Replayed) {
    let exercise = exercise(exercise_id);
    let text = |step: u32| {
        exercise
            .and_then(|e| e.steps.get(step as usize))
            .map_or("", |s| s.text.as_str())
    };
    for (at, step) in &replayed.steps {
        println!("{at:>8.1} s  Step {}: {}", step + 1, text(*step));
    }
    match &replayed.verdict {
        Verdict::Passed { line, .. } | Verdict::Failed { line, .. } => {
            println!("{:>8.1} s  {line}", replayed.at_s);
        }
        _ => println!("{:>8.1} s  Not over by the end of the tlog.", replayed.at_s),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uxduck_core::domain::practice::Offset;
    use uxduck_engine::practice::judge::{Verdict, place};

    use super::*;

    const HOME: (f64, f64) = (-35.363_261, 149.165_23);

    struct Tlog(Vec<u8>, u8);

    impl Tlog {
        fn send(&mut self, at_s: f64, system_id: u8, name: &str, fields: Value) {
            let Value::Object(fields) = fields else {
                panic!("fields")
            };
            let encoded = uxduck_mavlink::named::encode(name, &fields).unwrap();
            let frame = uxduck_mavlink::named::frame(&encoded, system_id, 1, self.1);
            self.1 = self.1.wrapping_add(1);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            uxduck_mavlink::tlog::record(&mut self.0, 1_000_000 + (at_s * 1e6) as u64, &frame)
                .unwrap();
        }

        /// The vehicle at a place, in a mode, with its battery.
        fn vehicle(&mut self, at_s: f64, at: (f64, f64), height_m: f64, mode: u32, battery: i8) {
            let armed = if height_m > 1.0 { 128 + 1 } else { 1 };
            self.send(at_s, 1, "HEARTBEAT", json!({ "type": 2, "autopilot": 3, "base_mode": armed, "custom_mode": mode, "system_status": 4, "mavlink_version": 3 }));
            self.send(
                at_s,
                1,
                "SYS_STATUS",
                json!({ "battery_remaining": battery }),
            );
            self.send(
                at_s,
                1,
                "EXTENDED_SYS_STATE",
                json!({ "landed_state": if height_m > 1.0 { 2 } else { 1 } }),
            );
            #[allow(clippy::cast_possible_truncation)]
            self.send(at_s, 1, "GLOBAL_POSITION_INT", json!({
                "lat": (at.0 * 1e7) as i32, "lon": (at.1 * 1e7) as i32, "relative_alt": (height_m * 1000.0) as i32,
            }));
        }
    }

    #[test]
    fn bring_it_home_replayed_passes_as_it_did_and_fails_without_return_to_launch() {
        let out = place(
            HOME,
            Offset {
                bearing_deg: 90.0,
                distance_m: 400.0,
            },
        );
        let flight = |rtl: bool| {
            let mut tlog = Tlog(Vec::new(), 0);
            tlog.send(
                0.0,
                255,
                "HEARTBEAT",
                json!({ "type": 6, "autopilot": 8, "mavlink_version": 3 }),
            );
            // The vehicle is heard first; home is asked of it after.
            tlog.send(
                0.0,
                1,
                "HEARTBEAT",
                json!({ "type": 2, "autopilot": 3, "base_mode": 129, "custom_mode": 5, "mavlink_version": 3 }),
            );
            #[allow(clippy::cast_possible_truncation)]
            tlog.send(
                0.5,
                1,
                "HOME_POSITION",
                json!({ "latitude": (HOME.0 * 1e7) as i32, "longitude": (HOME.1 * 1e7) as i32 }),
            );
            tlog.vehicle(1.0, out, 50.0, 5, 35);
            if rtl {
                tlog.send(
                    2.0,
                    255,
                    "COMMAND_LONG",
                    json!({ "target_system": 1, "command": 20 }),
                );
                tlog.vehicle(3.0, out, 50.0, 6, 34);
                tlog.vehicle(60.0, HOME, 0.0, 6, 28);
            } else {
                tlog.vehicle(60.0, out, 50.0, 5, 14);
            }
            tlog.0
        };
        let replayed = judge_tlog(&flight(true), "bring-it-home", 0.0).unwrap();
        assert_eq!(replayed.steps, [(1.0, 0), (3.0, 1)]);
        assert_eq!(
            replayed.verdict,
            Verdict::Passed {
                line: "Passed. Home with 28% left.".into(),
                measure: Some(28.0)
            }
        );
        let replayed = judge_tlog(&flight(false), "bring-it-home", 0.0).unwrap();
        assert!(
            matches!(replayed.verdict, Verdict::Failed { ref line, .. } if line == "Failed. The battery ran low with the vehicle still in the air."),
            "{replayed:?}"
        );
        assert!(judge_tlog(&flight(true), "no-such", 0.0).is_err());
    }

    #[test]
    fn parameter_puzzle_replayed_counts_the_station_s_parameter_and_fly_here() {
        let ring = place(
            HOME,
            Offset {
                bearing_deg: 300.0,
                distance_m: 300.0,
            },
        );
        let mut tlog = Tlog(Vec::new(), 0);
        tlog.vehicle(0.0, HOME, 50.0, 5, 90);
        tlog.send(5.0, 255, "PARAM_SET", json!({ "target_system": 1, "param_id": "WPNAV_SPEED", "param_value": 1000.0, "param_type": 9 }));
        tlog.vehicle(6.0, HOME, 50.0, 5, 90);
        #[allow(clippy::cast_possible_truncation)]
        tlog.send(7.0, 255, "COMMAND_INT", json!({ "target_system": 1, "command": 192, "x": (ring.0 * 1e7) as i32, "y": (ring.1 * 1e7) as i32, "z": 50.0 }));
        tlog.vehicle(40.0, ring, 50.0, 4, 88);
        let replayed = judge_tlog(&tlog.0, "parameter-puzzle", 0.0).unwrap();
        assert_eq!(
            replayed.verdict,
            Verdict::Passed {
                line: "Passed. At the ring in 40 s.".into(),
                measure: Some(40.0)
            }
        );
    }
}

//! PX4's health and arming checks (PX4 1.14 onwards), which it reports as
//! MAVLink events rather than text: a summary of the mode groups it can arm
//! in, one event per check with something to say, and a health summary that
//! ends the report. Each event is an id and packed arguments, worded by
//! PX4's events catalogue; `px4/events.json` keeps its arming and health
//! groups (`scripts/px4-events.py`), under PX4's licence beside it.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::LazyLock;

use serde::Deserialize;

/// `commander_arming_check_summary`: starts a report.
const ARMING_SUMMARY: u32 = 27_825_120;
/// `commander_health_summary`: ends it.
const HEALTH_SUMMARY: u32 = 18_691_879;
/// Events kept to put a report together and ask again for a lost one.
const KEPT: u16 = 64;

#[derive(Deserialize)]
struct Catalogue {
    events: HashMap<u32, Definition>,
    enums: HashMap<String, Enum>,
}

#[derive(Deserialize)]
struct Definition {
    message: String,
    description: String,
    arguments: Vec<String>,
}

#[derive(Deserialize)]
struct Enum {
    #[serde(rename = "type")]
    base: String,
    bitfield: bool,
    entries: BTreeMap<u64, String>,
}

static CATALOGUE: LazyLock<Catalogue> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../px4/events.json")).unwrap_or_else(|error| {
        panic!("px4/events.json is not PX4's events catalogue ({error}): run scripts/px4-events.py")
    })
});

/// One of PX4's checks, as worded for a person.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub text: String,
    /// Its longer explanation, when PX4's catalogue has one.
    pub description: String,
    /// `MAV_SEVERITY`: 0 emergency to 7 debug.
    pub severity: u8,
    /// The mode groups it concerns, one bit each.
    pub modes: u32,
    /// Whether it holds arming in the mode asked about: PX4 can't arm there,
    /// and the check names that mode or is an error. PX4 names every mode for
    /// an optional check, such as missing sticks, which holds only the modes
    /// that need it, and the report doesn't say which.
    pub holds: bool,
}

/// PX4's latest health and arming report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// The mode groups it can arm in, one bit each.
    pub can_arm: u32,
    pub checks: Vec<Check>,
}

impl Report {
    /// What it says about one mode (`custom_mode`), named `mode`.
    #[must_use]
    pub fn for_mode(&self, custom_mode: u32, mode: Option<String>) -> ArmingChecks {
        const ERROR: u8 = 3;
        let group = mode_group(custom_mode);
        let held = group.is_some_and(|group| self.can_arm & group == 0);
        let mut checks: Vec<Check> = self
            .checks
            .iter()
            .filter(|check| group.is_none_or(|group| check.modes & group != 0))
            .map(|check| Check {
                holds: held && (check.modes != u32::MAX || check.severity <= ERROR),
                ..check.clone()
            })
            .collect();
        checks.sort_by_key(|check| check.severity);
        ArmingChecks {
            mode,
            can_arm: group.map(|group| self.can_arm & group != 0),
            checks,
        }
    }
}

/// What PX4's report says about the mode it is in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArmingChecks {
    /// UXDuck's name for the mode.
    pub mode: Option<String>,
    /// Whether it can arm in it: unknown for a mode UXDuck cannot place.
    pub can_arm: Option<bool>,
    /// The checks that concern that mode, most serious first, or every
    /// check when the mode is unknown.
    pub checks: Vec<Check>,
}

/// What an event added.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Heard {
    Nothing,
    /// A whole report.
    Report(Report),
    /// A report with events lost on the way: ask for these again
    /// (`REQUEST_EVENT`).
    Missing {
        first: u16,
        last: u16,
    },
}

/// One vehicle's recent events, put together into reports.
#[derive(Clone, Debug, Default)]
pub struct Events {
    recent: VecDeque<(u16, u32, u8, [u8; 40])>,
    /// A report's end, waiting for the events asked for again.
    waiting: Option<u16>,
}

impl Events {
    /// An event, `EVENT`: its sequence, id, `log_levels` and arguments.
    pub fn event(&mut self, sequence: u16, id: u32, log_levels: u8, arguments: [u8; 40]) -> Heard {
        if self.recent.iter().any(|(seen, ..)| *seen == sequence) {
            return Heard::Nothing;
        }
        self.recent.push_back((sequence, id, log_levels, arguments));
        if self.recent.len() > usize::from(KEPT) {
            self.recent.pop_front();
        }
        let end = if id == HEALTH_SUMMARY {
            self.waiting = None;
            sequence
        } else if let Some(end) = self.waiting {
            end
        } else {
            return Heard::Nothing;
        };
        match self.report(end) {
            Ok(report) => {
                self.waiting = None;
                Heard::Report(report)
            }
            Err((first, last)) if id == HEALTH_SUMMARY => {
                self.waiting = Some(end);
                Heard::Missing { first, last }
            }
            Err(_) => Heard::Nothing,
        }
    }

    /// The report that ends at `end`, or the sequences missing from it.
    fn report(&self, end: u16) -> Result<Report, (u16, u16)> {
        let find = |sequence: u16| self.recent.iter().find(|(seen, ..)| *seen == sequence);
        let mut checks = Vec::new();
        let mut missing: Option<(u16, u16)> = None;
        for back in 1..KEPT {
            let sequence = end.wrapping_sub(back);
            let Some(&(_, id, log_levels, arguments)) = find(sequence) else {
                missing = Some((sequence, missing.map_or(sequence, |(_, last)| last)));
                continue;
            };
            if id == ARMING_SUMMARY {
                if let Some(gap) = missing {
                    return Err(gap);
                }
                checks.reverse();
                return Ok(Report {
                    can_arm: u32_at(&arguments, 9),
                    checks,
                });
            }
            if id == HEALTH_SUMMARY {
                break;
            }
            checks.push(check(id, log_levels, &arguments));
        }
        let last = end.wrapping_sub(1);
        Err(missing.unwrap_or((end.wrapping_sub(KEPT / 2), last)))
    }
}

/// The arming summary's `can_arm`, after its chunk index and two health
/// bitfields.
fn u32_at(arguments: &[u8; 40], at: usize) -> u32 {
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&arguments[at..at + 4]);
    u32::from_le_bytes(bytes)
}

/// A check event: its modes and health component, then its own arguments.
fn check(id: u32, log_levels: u8, arguments: &[u8; 40]) -> Check {
    let modes = u32_at(arguments, 0);
    let severity = log_levels & 0x0f;
    let Some(definition) = CATALOGUE.events.get(&id) else {
        return Check {
            text: format!("A PX4 check this UXDuck does not know (event {id})"),
            description: String::new(),
            severity,
            modes,
            holds: false,
        };
    };
    Check {
        text: worded(&definition.message, &definition.arguments, arguments),
        description: definition.description.clone(),
        severity,
        modes,
        holds: false,
    }
}

/// An argument's value, read as its type says.
enum Value {
    Integer(i64),
    Float(f32),
}

fn values(types: &[String], bytes: &[u8; 40]) -> Vec<Option<(Value, Option<&'static Enum>)>> {
    let mut at = 0;
    types
        .iter()
        .map(|kind| {
            let enumeration = CATALOGUE.enums.get(kind);
            let base = enumeration.map_or(kind.as_str(), |e| e.base.as_str());
            let size = match base {
                "uint8_t" | "int8_t" => 1,
                "uint16_t" | "int16_t" => 2,
                "uint32_t" | "int32_t" | "float" => 4,
                "uint64_t" | "int64_t" => 8,
                _ => return None,
            };
            let field = bytes.get(at..at + size)?;
            at += size;
            let mut raw = [0u8; 8];
            raw[..size].copy_from_slice(field);
            let unsigned = u64::from_le_bytes(raw);
            let value = match base {
                "float" => Value::Float(f32::from_bits(u32::try_from(unsigned).ok()?)),
                "int8_t" => Value::Integer(i64::from(i8::from_le_bytes([field[0]]))),
                "int16_t" => Value::Integer(i64::from(i16::from_le_bytes([field[0], field[1]]))),
                "int32_t" => Value::Integer(i64::from(i32::from_le_bytes([
                    field[0], field[1], field[2], field[3],
                ]))),
                _ => Value::Integer(i64::try_from(unsigned).ok()?),
            };
            Some((value, enumeration))
        })
        .collect()
}

/// A catalogue message with its arguments put in: `{3}` for the third, and
/// `{3:.1}` or `{3:.1m/s}` for a number with one decimal and a unit.
fn worded(message: &str, types: &[String], bytes: &[u8; 40]) -> String {
    let values = values(types, bytes);
    let mut out = String::new();
    let mut rest = message;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let spec = &rest[open + 1..open + close];
        rest = &rest[open + close + 1..];
        let (index, style) = spec.split_once(':').unwrap_or((spec, ""));
        let value = index
            .parse::<usize>()
            .ok()
            .and_then(|n| values.get(n.checked_sub(1)?))
            .and_then(Option::as_ref);
        match value {
            Some((value, enumeration)) => out.push_str(&shown(value, *enumeration, style)),
            None => out.push('?'),
        }
    }
    out.push_str(rest);
    out
}

fn shown(value: &Value, enumeration: Option<&Enum>, style: &str) -> String {
    let digits = style.strip_prefix('.').map(|style| {
        let end = style
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(style.len());
        (
            style[..end].parse::<usize>().unwrap_or(0),
            style[end..].trim(),
        )
    });
    match (value, enumeration, digits) {
        (&Value::Integer(n), Some(enumeration), _) => {
            let Ok(n) = u64::try_from(n) else {
                return n.to_string();
            };
            if enumeration.bitfield {
                let names: Vec<&str> = enumeration
                    .entries
                    .iter()
                    .filter(|(bit, _)| n & **bit != 0)
                    .map(|(_, name)| name.as_str())
                    .collect();
                names.join(", ")
            } else {
                enumeration
                    .entries
                    .get(&n)
                    .cloned()
                    .unwrap_or_else(|| n.to_string())
            }
        }
        (&Value::Float(x), _, Some((digits, unit))) => with_unit(format!("{x:.digits$}"), unit),
        (&Value::Float(x), _, None) => x.to_string(),
        (&Value::Integer(n), None, digits) => with_unit(n.to_string(), digits.map_or("", |d| d.1)),
    }
}

fn with_unit(number: String, unit: &str) -> String {
    if unit.is_empty() {
        number
    } else {
        format!("{number} {unit}")
    }
}

/// PX4's mode group for a custom mode, as its report counts them: one bit
/// per navigation state (`vehicle_status`'s `NAVIGATION_STATE_*`, PX4 1.14
/// to 1.17). None for a mode it does not place.
#[must_use]
pub fn mode_group(custom_mode: u32) -> Option<u32> {
    let main = (custom_mode >> 16) & 0xff;
    let sub = custom_mode >> 24;
    let state = match (main, sub) {
        (1, _) => 0,
        (2, _) => 1,
        (3, 0) => 2,
        (3, 1) => 21,
        (3, 2) => 6,
        (4, 2) => 17,
        (4, 3) => 4,
        (4, 4) => 3,
        (4, 5) => 5,
        (4, 6) => 18,
        (4, 8) => 19,
        (4, 9) => 20,
        (4, 10) => 22,
        (4, 11..=18) => sub + 12,
        (5, _) => 10,
        (6, _) => 14,
        (7, _) => 15,
        _ => return None,
    };
    Some(1 << state)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOLD: u32 = 4 << 16 | 3 << 24;
    const POSITION: u32 = 3 << 16;

    /// An event's id in PX4's catalogue, by its name.
    fn id_of(name: &str) -> u32 {
        let catalogue: serde_json::Value =
            serde_json::from_str(include_str!("../px4/events.json")).unwrap();
        catalogue["events"]
            .as_object()
            .unwrap()
            .iter()
            .find(|(_, event)| event["name"] == name)
            .and_then(|(id, _)| id.parse().ok())
            .unwrap_or_else(|| panic!("no {name} in px4/events.json"))
    }

    fn args(parts: &[&[u8]]) -> [u8; 40] {
        let mut out = [0u8; 40];
        let mut at = 0;
        for part in parts {
            out[at..at + part.len()].copy_from_slice(part);
            at += part.len();
        }
        out
    }

    fn summary(can_arm: u32) -> [u8; 40] {
        args(&[
            &[0],
            &0u32.to_le_bytes(),
            &0u32.to_le_bytes(),
            &can_arm.to_le_bytes(),
            &can_arm.to_le_bytes(),
        ])
    }

    fn check_args(modes: u32, component: u8, rest: &[u8]) -> [u8; 40] {
        args(&[&modes.to_le_bytes(), &[component], rest])
    }

    /// A report as PX4 1.17 sent it, disarmed in Hold with no stick input,
    /// from `first` on.
    fn report(events: &mut Events, first: u16) -> Vec<Heard> {
        // Hold, Return, Take off and Land among the modes it can arm in;
        // Manual, Altitude, Position and Mission not.
        let can_arm = 2_155_821_744;
        let manual = id_of("check_modes_manual_control");
        let offboard = id_of("check_modes_offboard_signal");
        [
            (ARMING_SUMMARY, 0x88, summary(can_arm)),
            (id_of("check_modes_mission"), 0x66, check_args(8, 20, &[])),
            (offboard, 0x33, check_args(16384, 20, &[])),
            (manual, 0x44, check_args(u32::MAX, 7, &[])),
            (HEALTH_SUMMARY, 0x88, args(&[&[0]])),
        ]
        .into_iter()
        .enumerate()
        .map(|(n, (id, levels, arguments))| {
            #[allow(clippy::cast_possible_truncation, reason = "five events")]
            let sequence = first.wrapping_add(n as u16);
            events.event(sequence, id, levels, arguments)
        })
        .collect()
    }

    #[test]
    fn the_catalogue_has_the_summaries_this_reads() {
        assert_eq!(id_of("commander_arming_check_summary"), ARMING_SUMMARY);
        assert_eq!(id_of("commander_health_summary"), HEALTH_SUMMARY);
        assert!(CATALOGUE.events.len() > 100, "{}", CATALOGUE.events.len());
    }

    #[test]
    fn a_report_says_what_holds_the_mode_it_is_in() {
        let mut events = Events::default();
        let heard = report(&mut events, 65534);
        assert!(heard[..4].iter().all(|h| *h == Heard::Nothing), "{heard:?}");
        let Heard::Report(report) = &heard[4] else {
            panic!("no report: {heard:?}");
        };
        assert_eq!(report.checks.len(), 3);

        let hold = report.for_mode(HOLD, Some("Hold".into()));
        assert_eq!(hold.can_arm, Some(true));
        let texts: Vec<&str> = hold.checks.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(
            texts,
            ["No manual control input"],
            "only what concerns Hold"
        );
        assert_eq!(hold.checks[0].severity, 4);
        assert_eq!(
            hold.checks[0].description,
            "Connect and enable stick input or use autonomous mode."
        );

        assert!(!hold.checks[0].holds);

        let position = report.for_mode(POSITION, Some("Position".into()));
        assert_eq!(position.can_arm, Some(false), "Position needs sticks");
        // Missing sticks hold Position, but PX4 names every mode for them:
        // only a warning, as the report can't say more.
        assert!(!position.checks[0].holds, "{position:?}");
        let mission = report.for_mode(4 << 16 | 4 << 24, Some("Mission".into()));
        let holding: Vec<&str> = mission
            .checks
            .iter()
            .filter(|c| c.holds)
            .map(|c| c.text.as_str())
            .collect();
        assert_eq!(holding, ["No valid mission available"], "{mission:?}");
        let unknown = report.for_mode(9 << 16, None);
        assert_eq!(unknown.can_arm, None);
        assert_eq!(unknown.checks.len(), 3, "every check, most serious first");
        assert_eq!(unknown.checks[0].severity, 3);
    }

    #[test]
    fn a_lost_event_is_asked_for_again_and_completes_the_report() {
        let mut events = Events::default();
        let manual = id_of("check_modes_manual_control");
        assert_eq!(
            events.event(10, ARMING_SUMMARY, 0x88, summary(16)),
            Heard::Nothing
        );
        assert_eq!(
            events.event(12, HEALTH_SUMMARY, 0x88, args(&[&[0]])),
            Heard::Missing {
                first: 11,
                last: 11
            }
        );
        let Heard::Report(report) = events.event(11, manual, 0x44, check_args(u32::MAX, 7, &[]))
        else {
            panic!("the resent event completes it");
        };
        assert_eq!(report.can_arm, 16);
        assert_eq!(report.checks[0].text, "No manual control input");
        assert_eq!(
            events.event(11, manual, 0x44, check_args(u32::MAX, 7, &[])),
            Heard::Nothing
        );
    }

    #[test]
    fn a_report_heard_from_its_end_asks_for_what_came_before() {
        let mut events = Events::default();
        let Heard::Missing { first, last } = events.event(100, HEALTH_SUMMARY, 0x88, args(&[&[0]]))
        else {
            panic!("asks again");
        };
        assert_eq!((first, last), (37, 99));
    }

    #[test]
    fn arguments_are_worded_as_the_catalogue_says() {
        let check = |name: &str, rest: &[u8]| -> String {
            super::check(id_of(name), 0x33, &check_args(u32::MAX, 11, rest)).text
        };
        assert_eq!(check("check_battery_missing", &[2]), "Battery 2 missing");
        assert_eq!(
            check("check_cpuload_too_high", &95.3f32.to_le_bytes()),
            "CPU load too high: 95.3%"
        );
        assert_eq!(
            check("check_wind_too_high", &12.46f32.to_le_bytes()),
            "Wind speed is above limit (12.5 m/s)"
        );
        assert_eq!(
            check("check_escs_fault", &[1, 2, 0]),
            "ESC 1: Motor has reached a critical temperature"
        );
        let unknown = super::check(0x0100_0001, 0x33, &check_args(1, 0, &[]));
        assert_eq!(
            unknown.text,
            "A PX4 check this UXDuck does not know (event 16777217)"
        );
    }

    #[test]
    fn each_mode_has_its_group() {
        assert_eq!(mode_group(1 << 16), Some(1), "Manual");
        assert_eq!(mode_group(POSITION), Some(4));
        assert_eq!(mode_group(HOLD), Some(16));
        assert_eq!(mode_group(4 << 16 | 2 << 24), Some(1 << 17), "Takeoff");
        assert_eq!(mode_group(4 << 16 | 11 << 24), Some(1 << 23), "External 1");
        assert_eq!(mode_group(4 << 16 | 1 << 24), None, "Ready");
    }
}

//! A battery's own alert (UXD-253): what the autopilot says of each of a
//! vehicle's batteries.
//!
//! The autopilot says it three ways:
//!
//! - `BATTERY_STATUS`'s charge state, `<group>.charge_state` (`ArduPilot`
//!   4.1 on, PX4), from the autopilot's component alone (`Session`);
//! - `ArduPilot`'s "Battery 1 is low 10.80V used 1234 mAh", which it says
//!   once, while armed, as its battery failsafe starts;
//! - `SYS_STATUS`'s battery bit, `battery.healthy`, read only from an
//!   autopilot that gives no charge state. `ArduPilot` clears it for any
//!   battery failsafe and keeps it cleared until it restarts, so after a
//!   sign of one the bit says nothing until it reads healthy again, and it
//!   holds as long as the battery alarm does before it is a notice.
//!
//! A fault, from the charge state or the bit, is raised once it has held
//! that long and resolved once the battery has read well as long, so a
//! reading that flickers does not flap.
//!
//! Low and critical are not a notice of their own: they are more triggers of
//! the telemetry alarms' "Battery low" and "Battery critical" (S369), beside
//! the operator's shares and the vehicle's own voltages, so a vehicle has one
//! battery alarm, held 3 s, raised while armed and kept until disarming.
//! A battery that has failed, or is unhealthy, which those alarms have no
//! kind for, is an alarm of its own, one per battery, resolved when the
//! autopilot says it is well again. `ArduPilot`'s "Battery 2 is missing"
//! stays a message: nothing says when such a battery is back but its
//! charge state, which raises and resolves its own notice.
//!
//! What the Cockpit's status line and the battery tiles show is said as
//! `<group>.alert` (`battery.alert`, `battery_2.alert`): the words, with 1
//! for low and 2 for critical, failed or unhealthy.

use std::collections::{BTreeMap, BTreeSet};

use uxduck_core::domain::notification::{NotificationKind, Severity, Subject};
use uxduck_core::domain::telemetry::{TelemetryItem, Unit};

use crate::Engine;
use crate::commands::Raise;
use crate::telemetry_alarms::BATTERY_HOLD_US;

/// A charge state older than this says nothing, as for the telemetry alarms.
const FRESH_US: u64 = 5_000_000;

/// How low a battery is, as its autopilot says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Charge {
    Low,
    Critical,
    /// PX4's emergency, below critical: the critical alarm.
    Emergency,
}

impl Charge {
    const fn words(self) -> &'static str {
        match self {
            Self::Low => "is low",
            Self::Critical => "is critical",
            Self::Emergency => "is at its emergency level",
        }
    }
}

/// A battery that is more than low, as its autopilot says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fault {
    Failed,
    Unhealthy,
}

impl Fault {
    const fn words(self) -> &'static str {
        match self {
            Self::Failed => "failed",
            Self::Unhealthy => "unhealthy",
        }
    }
}

/// `MAV_BATTERY_CHARGE_STATE`, of what UXDuck acts on.
enum State {
    Charge(Charge),
    Fault(Fault),
    Well,
}

fn state(value: f64) -> State {
    match value.round() {
        2.0 => State::Charge(Charge::Low),
        3.0 => State::Charge(Charge::Critical),
        4.0 => State::Charge(Charge::Emergency),
        5.0 => State::Fault(Fault::Failed),
        6.0 => State::Fault(Fault::Unhealthy),
        _ => State::Well,
    }
}

/// What `ArduPilot` said of a battery failsafe as it started.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Said {
    charge: Charge,
    volts: Option<f64>,
    used: Option<f64>,
}

/// Each vehicle's batteries' faults raised and the readings that drive
/// them, `ArduPilot`'s word on a battery failsafe, kept until disarming as
/// the failsafe is, and the vehicles that have shown a sign of a battery
/// failsafe since their `SYS_STATUS` battery bit was last healthy. Not
/// journaled: the notifications they raised are.
#[derive(Debug, Default)]
pub(crate) struct Batteries {
    faults: BTreeMap<(String, u8), Fault>,
    readings: BTreeMap<(String, u8), Reading>,
    said: BTreeMap<(String, u8), Said>,
    failsafe_seen: BTreeSet<String>,
}

/// A battery's latest fault reading, from its charge state or the
/// `SYS_STATUS` bit: the fault or none, when it came, since when it has
/// said a fault, or none, without a break, and whether the bit said it.
#[derive(Debug, Clone, Copy)]
struct Reading {
    fault: Option<Fault>,
    at: u64,
    since: u64,
    bit: bool,
}

/// What `ArduPilot`'s failsafe message says of a battery.
enum Failsafe {
    Charge(Charge),
    /// "missing": no healthy reading for 5 s.
    Missing,
}

/// `ArduPilot`'s word on a battery failsafe starting: the battery's id
/// (its number less one), low, critical or missing, and its voltage and mAh
/// used. `"Battery 1 is low 10.80V used 1234 mAh"`, `"... is critical ..."`
/// and `"... is missing, last: 0.00V used 0 mAh"`.
fn failsafe_text(text: &str) -> Option<(u8, Failsafe, Option<f64>, Option<f64>)> {
    let rest = text.trim().strip_prefix("Battery ")?;
    let (number, rest) = rest.split_once(" is ")?;
    let id = number.parse::<u8>().ok()?.checked_sub(1)?;
    let (level, rest) = [
        ("low ", Failsafe::Charge(Charge::Low)),
        ("critical ", Failsafe::Charge(Charge::Critical)),
        ("missing, last: ", Failsafe::Missing),
    ]
    .into_iter()
    .find_map(|(word, level)| rest.strip_prefix(word).map(|rest| (level, rest)))?;
    let volts = rest
        .split_whitespace()
        .next()
        .and_then(|v| v.strip_suffix('V'))
        .and_then(|v| v.parse().ok());
    let used = rest
        .split_once("used ")
        .and_then(|(_, used)| used.split_whitespace().next())
        .and_then(|mah| mah.parse().ok());
    Some((id, level, volts, used))
}

/// A battery's group and name, as the producers key its items:
/// `battery` and "Battery" for the first, `battery_2` and "Battery 2" on.
fn group(id: u8) -> (String, String) {
    if id == 0 {
        ("battery".to_owned(), "Battery".to_owned())
    } else {
        let n = u16::from(id) + 1;
        (format!("battery_{n}"), format!("Battery {n}"))
    }
}

/// The battery's id from an item's group: `battery` 0, `battery_2` 1.
fn id_of(group: &str) -> Option<u8> {
    if group == "battery" {
        return Some(0);
    }
    let n: u16 = group.strip_prefix("battery_")?.parse().ok()?;
    u8::try_from(n.checked_sub(1)?).ok().filter(|id| *id > 0)
}

fn number(value: f64) -> String {
    let rounded = (value * 100.0).round() / 100.0;
    format!("{rounded}")
}

impl Engine {
    /// What a vehicle's telemetry says of its batteries' faults, from the
    /// items this update carries.
    pub(crate) fn review_batteries(&mut self, vehicle_id: &str, items: &[TelemetryItem]) {
        let mut stated = false;
        for item in items {
            let Some(group) = item.key.strip_suffix(".charge_state") else {
                continue;
            };
            let (Some(id), Some(value)) = (id_of(group), item.value) else {
                continue;
            };
            stated = true;
            let fault = match state(value) {
                State::Fault(fault) => Some(fault),
                State::Charge(_) | State::Well => None,
            };
            self.fault_reading(vehicle_id, id, fault, false);
        }
        let bit = items
            .iter()
            .find(|i| i.key == "battery.healthy")
            .and_then(|i| i.value);
        let charge_state_known = stated
            || self.telemetry.get(vehicle_id).is_some_and(|t| {
                t.items
                    .iter()
                    .any(|i| i.key.ends_with(".charge_state") && i.value.is_some())
            });
        if let Some(healthy) = bit.filter(|_| !charge_state_known) {
            self.health_bit(vehicle_id, healthy >= 0.5);
        }
        self.drive_faults(vehicle_id);
        self.say_battery_alerts(vehicle_id);
    }

    /// A battery's fault reading as it comes: it starts a new run when it
    /// goes from a fault to none, or back.
    fn fault_reading(&mut self, vehicle_id: &str, id: u8, fault: Option<Fault>, bit: bool) {
        let now = self.clock.now_us();
        let reading = self
            .batteries
            .readings
            .entry((vehicle_id.to_owned(), id))
            .or_insert(Reading {
                fault,
                at: now,
                since: now,
                bit,
            });
        if reading.fault.is_some() != fault.is_some() {
            reading.since = now;
        }
        reading.fault = fault;
        reading.at = now;
        reading.bit = bit;
    }

    /// A sign of a battery failsafe: the bit's unhealthy run, which the
    /// failsafe explains, is dropped unless its notice is already up.
    fn battery_failsafe_seen(&mut self, vehicle_id: &str) {
        self.batteries.failsafe_seen.insert(vehicle_id.to_owned());
        let key = (vehicle_id.to_owned(), 0);
        if !self.batteries.faults.contains_key(&key)
            && self.batteries.readings.get(&key).is_some_and(|r| r.bit)
        {
            self.batteries.readings.remove(&key);
        }
    }

    /// Raises, changes or resolves each of a vehicle's battery faults from
    /// its readings, on every telemetry update, so a hold ends on time
    /// between battery frames. A fault is raised once its reading has said
    /// it for `BATTERY_HOLD_US` without a break, and resolved once it has
    /// read well as long, so a reading that flickers does not flap: a flip
    /// back within the hold keeps the notice as it is. A reading older
    /// than `FRESH_US` says nothing, so a battery that falls silent keeps
    /// its notice, and a fault said once and never again is not raised.
    fn drive_faults(&mut self, vehicle_id: &str) {
        let now = self.clock.now_us();
        let readings: Vec<(u8, Reading)> = self
            .batteries
            .readings
            .iter()
            .filter(|((v, _), r)| v == vehicle_id && now.saturating_sub(r.at) <= FRESH_US)
            .map(|((_, id), r)| (*id, *r))
            .collect();
        for (id, reading) in readings {
            let key = (vehicle_id.to_owned(), id);
            let raised = self.batteries.faults.get(&key).copied();
            let held = now.saturating_sub(reading.since) >= BATTERY_HOLD_US;
            match (reading.fault, raised) {
                (Some(fault), Some(shown)) if fault != shown => {
                    self.battery_fault(vehicle_id, id, Some(fault));
                }
                (Some(fault), None) if held => self.battery_fault(vehicle_id, id, Some(fault)),
                (None, Some(_)) if held => {
                    self.battery_fault(vehicle_id, id, None);
                    self.batteries.readings.remove(&key);
                }
                (None, None) => {
                    self.batteries.readings.remove(&key);
                }
                _ => {}
            }
        }
    }

    /// `SYS_STATUS`'s battery bit from an autopilot that gives no charge
    /// state. `ArduPilot` sends `healthy() && !has_failsafed()`, and its
    /// failsafe stays until it restarts, so after a sign of one (the
    /// battery alarm, its failsafe message, or the bit clearing while the
    /// battery is below its levels) the bit says nothing until it is
    /// healthy again, and an unhealthy run it had begun is dropped.
    /// Otherwise it is the first battery's fault reading, held as a charge
    /// state's is.
    fn health_bit(&mut self, vehicle_id: &str, healthy: bool) {
        let id = vehicle_id.to_owned();
        if healthy {
            self.batteries.failsafe_seen.remove(&id);
            self.fault_reading(vehicle_id, 0, None, true);
            return;
        }
        if self.battery_alarm(vehicle_id).is_some() || self.battery_below_its_levels(vehicle_id) {
            self.battery_failsafe_seen(vehicle_id);
        }
        if self.batteries.failsafe_seen.contains(&id) {
            return;
        }
        self.fault_reading(vehicle_id, 0, Some(Fault::Unhealthy), true);
    }

    /// A vehicle's message: true when it was `ArduPilot`'s word on a low or
    /// critical battery, which trips the battery alarm instead of being a
    /// notice of its own. Any battery failsafe it names is a sign for the
    /// `SYS_STATUS` bit. "missing", and any of them disarmed, when the
    /// battery alarm is not watched, stay messages like any other.
    pub(crate) fn battery_message(&mut self, vehicle_id: &str, text: &str) -> bool {
        let Some((id, level, volts, used)) = failsafe_text(text) else {
            return false;
        };
        self.battery_failsafe_seen(vehicle_id);
        match level {
            Failsafe::Missing => return false,
            Failsafe::Charge(charge) => {
                if !self.vehicle(vehicle_id).is_some_and(|v| v.armed) {
                    return false;
                }
                let key = (vehicle_id.to_owned(), id);
                if self
                    .batteries
                    .said
                    .get(&key)
                    .is_none_or(|said| said.charge < charge)
                {
                    self.batteries.said.insert(
                        key,
                        Said {
                            charge,
                            volts,
                            used,
                        },
                    );
                }
                self.review_telemetry_alarms(vehicle_id);
            }
        }
        self.say_battery_alerts(vehicle_id);
        true
    }

    /// Disarmed: `ArduPilot`'s word on its battery failsafe is done with,
    /// as the battery alarm is.
    pub(crate) fn forget_battery_failsafe(&mut self, vehicle_id: &str) {
        self.batteries.said.retain(|(v, _), _| v != vehicle_id);
    }

    /// The battery the autopilot says is the lowest, low or worse, from a
    /// fresh charge state or its failsafe's words: its id, how low and what
    /// it said.
    fn autopilot_charge(&self, vehicle_id: &str, now: u64) -> Option<(u8, Charge, Option<Said>)> {
        let stated = self
            .telemetry
            .get(vehicle_id)
            .into_iter()
            .flat_map(|t| t.items.iter())
            .filter(|i| now.saturating_sub(i.time_us) <= FRESH_US)
            .filter_map(|i| {
                let id = id_of(i.key.strip_suffix(".charge_state")?)?;
                match state(i.value?) {
                    State::Charge(charge) => Some((id, charge, None)),
                    State::Fault(_) | State::Well => None,
                }
            });
        let said = self
            .batteries
            .said
            .iter()
            .filter(|((v, _), _)| v == vehicle_id)
            .map(|((_, id), said)| (*id, said.charge, Some(*said)));
        stated
            .chain(said)
            .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
    }

    /// Whether the autopilot gives a fresh charge state, so a battery alarm
    /// it has not tripped is known to be well by it.
    pub(crate) fn autopilot_states_charge(&self, vehicle_id: &str, now: u64) -> bool {
        self.telemetry.get(vehicle_id).is_some_and(|t| {
            t.items.iter().any(|i| {
                i.key.ends_with(".charge_state")
                    && i.value.is_some()
                    && now.saturating_sub(i.time_us) <= FRESH_US
            })
        })
    }

    /// The battery alarm's words when the autopilot trips it: low trips at
    /// low or worse, critical at critical or worse. "The autopilot says
    /// Battery 2 is critical. 10.8 V, 1234 mAh used.", the first battery
    /// being Battery 1 as `ArduPilot` numbers it.
    pub(crate) fn autopilot_battery_trip(
        &self,
        vehicle_id: &str,
        critical: bool,
        now: u64,
    ) -> Option<String> {
        let (id, charge, said) = self.autopilot_charge(vehicle_id, now)?;
        if critical && charge == Charge::Low {
            return None;
        }
        let (group, _) = group(id);
        let number = u16::from(id) + 1;
        let mut body = format!("The autopilot says Battery {number} {}.", charge.words());
        if let Some(numbers) = self.battery_numbers(vehicle_id, &group, said) {
            body.push(' ');
            body.push_str(&numbers);
        }
        Some(body)
    }

    /// A battery's fault now, as `drive_faults` decides: raised as an alarm,
    /// changed in place, or resolved. The same fault again changes nothing.
    fn battery_fault(&mut self, vehicle_id: &str, id: u8, fault: Option<Fault>) {
        let Some(vehicle) = self.vehicle(vehicle_id).cloned() else {
            return;
        };
        let key = (vehicle.id.clone(), id);
        let before = match fault {
            Some(fault) => self.batteries.faults.insert(key, fault),
            None => self.batteries.faults.remove(&key),
        };
        if before == fault {
            return;
        }
        let condition = format!("battery:{}:{id}", vehicle.id);
        let Some(fault) = fault else {
            self.resolve(&condition);
            return;
        };
        let (group, name) = group(id);
        let mut body = Vec::new();
        if let Some(numbers) = self.battery_numbers(&vehicle.id, &group, None) {
            body.push(numbers);
        }
        let faults = self
            .telemetry
            .get(&vehicle.id)
            .and_then(|t| t.items.iter().find(|i| i.key == format!("{group}.faults")))
            .and_then(|i| i.text.clone());
        match faults {
            Some(faults) => body.push(format!("Faults: {faults}.")),
            None if fault == Fault::Unhealthy => {
                body.push("The autopilot says it is not healthy.".to_owned());
            }
            None => {}
        }
        body.push("Land as soon as it is safe.".to_owned());
        self.raise_or_update(Raise {
            severity: Severity::Alarm,
            kind: NotificationKind::Persistent,
            subject: Subject::Vehicle {
                id: vehicle.id.clone(),
            },
            title: format!("{}: {name} {}", vehicle.name, fault.words()),
            body: Some(body.join(" ")),
            actions: Vec::new(),
            condition: Some(condition),
            report: None,
        });
    }

    /// Says each battery's `<group>.alert` as the alarms stand: the battery
    /// alarm on the battery that tripped it, the first battery when the
    /// vehicle's own levels did, and a battery's fault on it. Unchanged,
    /// nothing.
    pub(crate) fn say_battery_alerts(&mut self, vehicle_id: &str) {
        let now = self.clock.now_us();
        let mut alerts: BTreeMap<u8, (u8, &'static str)> = BTreeMap::new();
        if let Some(critical) = self.battery_alarm(vehicle_id) {
            let id = self
                .autopilot_charge(vehicle_id, now)
                .filter(|(_, charge, _)| critical || *charge == Charge::Low)
                .map_or(0, |(id, _, _)| id);
            alerts.insert(
                id,
                if critical {
                    (2, "critical")
                } else {
                    (1, "low")
                },
            );
        }
        for ((v, id), fault) in &self.batteries.faults {
            if v == vehicle_id {
                alerts.insert(*id, (2, fault.words()));
            }
        }
        let shown: Vec<u8> = self
            .telemetry
            .get(vehicle_id)
            .into_iter()
            .flat_map(|t| t.items.iter())
            .filter(|i| i.value.is_some())
            .filter_map(|i| id_of(i.key.strip_suffix(".alert")?))
            .collect();
        let ids: std::collections::BTreeSet<u8> = alerts.keys().copied().chain(shown).collect();
        for id in ids {
            let (group, name) = group(id);
            let alert = alerts.get(&id);
            self.telemetry_said(
                vehicle_id,
                TelemetryItem {
                    key: format!("{group}.alert"),
                    label: format!("{name} alert"),
                    unit: Unit::None,
                    value: alert.map(|(rank, _)| f64::from(*rank)),
                    text: alert.map(|(_, words)| (*words).to_owned()),
                    source: "battery".into(),
                    component_id: 1,
                    time_us: now,
                },
            );
        }
    }

    /// "10.8 V, 1234 mAh used.": the autopilot's own numbers when its
    /// message gave them, else the battery's telemetry now.
    fn battery_numbers(&self, vehicle_id: &str, group: &str, said: Option<Said>) -> Option<String> {
        let items = self.telemetry.get(vehicle_id).map(|t| t.items.as_slice());
        let reading = |key: &str| {
            items
                .and_then(|items| items.iter().find(|i| i.key == key))
                .and_then(|i| i.value)
        };
        let cells: Option<f64> = items.map(|items| {
            items
                .iter()
                .filter(|i| {
                    i.key
                        .strip_prefix(group)
                        .and_then(|k| k.strip_prefix(".cell_"))
                        .and_then(|k| k.strip_suffix("_v"))
                        .is_some_and(|n| n.parse::<u8>().is_ok())
                })
                .filter_map(|i| i.value)
                .sum()
        });
        let (volts, used) = said.map_or_else(
            || {
                (
                    reading(&format!("{group}.voltage_v")).or_else(|| cells.filter(|v| *v > 0.0)),
                    reading(&format!("{group}.consumed_mah")),
                )
            },
            |said| (said.volts, said.used),
        );
        let mut parts = Vec::new();
        if let Some(volts) = volts {
            parts.push(format!("{} V", number(volts)));
        }
        if let Some(used) = used {
            parts.push(format!("{} mAh used", used.round()));
        }
        (!parts.is_empty()).then(|| format!("{}.", parts.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ardupilots_failsafe_words_are_read() {
        let read = |text| {
            failsafe_text(text).map(|(id, level, volts, used)| {
                let level = match level {
                    Failsafe::Charge(charge) => Some(charge),
                    Failsafe::Missing => None,
                };
                (id, level, volts, used)
            })
        };
        assert_eq!(
            read("Battery 1 is low 10.80V used 1234 mAh"),
            Some((0, Some(Charge::Low), Some(10.8), Some(1234.0)))
        );
        assert_eq!(
            read("Battery 2 is critical 9.90V used 2200 mAh"),
            Some((1, Some(Charge::Critical), Some(9.9), Some(2200.0)))
        );
        assert_eq!(
            read("Battery 1 is missing, last: 0.00V used 0 mAh"),
            Some((0, None, Some(0.0), Some(0.0)))
        );
        assert_eq!(read("Battery 0 is low 10.80V used 1 mAh"), None);
        assert_eq!(read("Battery failsafe"), None);
        assert_eq!(read("PreArm: Battery 1 low voltage failsafe"), None);
    }

    #[test]
    fn a_batterys_group_and_id_go_both_ways() {
        for id in [0, 1, 7, 254] {
            assert_eq!(id_of(&group(id).0), Some(id));
        }
        assert_eq!(group(1).1, "Battery 2");
        assert_eq!(id_of("battery_1"), None);
        assert_eq!(id_of("battery_x"), None);
    }
}

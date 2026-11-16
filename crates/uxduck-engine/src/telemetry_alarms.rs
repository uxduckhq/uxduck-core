//! Telemetry alarms (ADR-0005, amended 2026-11-06): what UXDuck raises
//! itself from an armed vehicle's telemetry, so a lost GPS fix, a low
//! battery, a bad position estimate, a failsafe or a fence breach is shown,
//! toned and said aloud even when the autopilot sends no message about it.
//! Each is bound to a condition, `telemetry:<kind>:<vehicle>`, raised once
//! it has held for a moment and resolved once it has cleared for a moment,
//! so a reading that flickers does not flap. Disarming resolves them all.

use std::collections::BTreeMap;

use uxduck_core::domain::{
    notification::{NotificationKind, NotificationState, Severity, Subject},
    parameter::parameter_id,
    telemetry::TelemetryItem,
    vehicle::Vehicle,
};

use crate::{Engine, commands::Raise};

/// A reading older than this says nothing.
const FRESH_US: u64 = 5_000_000;
/// `GPS_FIX_TYPE_3D_FIX`.
const GPS_3D_FIX: f64 = 3.0;
/// A 3D fix with fewer satellites than this, or a worse HDOP, is weak.
const FEW_SATELLITES: f64 = 6.0;
const POOR_HDOP: f64 = 2.0;
const SECOND: u64 = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    GpsLost,
    GpsWeak,
    BatteryLow,
    BatteryCritical,
    PositionBad,
    Failsafe,
    Fence,
}

impl Kind {
    const ALL: [Self; 7] = [
        Self::GpsLost,
        Self::GpsWeak,
        Self::BatteryLow,
        Self::BatteryCritical,
        Self::PositionBad,
        Self::Failsafe,
        Self::Fence,
    ];

    const fn id(self) -> &'static str {
        match self {
            Self::GpsLost => "gps-lost",
            Self::GpsWeak => "gps-weak",
            Self::BatteryLow => "battery-low",
            Self::BatteryCritical => "battery-critical",
            Self::PositionBad => "position-bad",
            Self::Failsafe => "failsafe",
            Self::Fence => "fence",
        }
    }

    /// How long it holds before it is raised, and how long it is clear
    /// before it is resolved; a battery stays raised until disarming, as
    /// the autopilot's own battery failsafe does.
    const fn timing(self) -> (u64, Option<u64>) {
        match self {
            Self::GpsLost | Self::PositionBad => (2 * SECOND, Some(3 * SECOND)),
            Self::GpsWeak => (5 * SECOND, Some(5 * SECOND)),
            Self::BatteryLow | Self::BatteryCritical => (3 * SECOND, None),
            Self::Failsafe => (SECOND, Some(3 * SECOND)),
            Self::Fence => (0, Some(2 * SECOND)),
        }
    }

    const fn severity(self) -> Severity {
        match self {
            Self::GpsWeak => Severity::Warning,
            _ => Severity::Alarm,
        }
    }

    fn condition(self, vehicle_id: &str) -> String {
        format!("telemetry:{}:{vehicle_id}", self.id())
    }
}

/// Since when each alarm's reading has been bad, or good, and which are
/// raised.
#[derive(Debug, Default)]
struct Watch {
    bad_since: Option<u64>,
    good_since: Option<u64>,
    /// Raised and not resolved since; dismissed by a person, it is not
    /// raised again until it has cleared.
    raised: bool,
}

/// Each vehicle's watches, and what it has shown it can do: an alarm about
/// losing a GPS fix or a position needs a vehicle that had one.
#[derive(Debug, Default)]
pub(crate) struct TelemetryAlarms {
    watches: BTreeMap<(String, Kind), Watch>,
    had_fix: std::collections::BTreeSet<String>,
    had_position: std::collections::BTreeSet<String>,
}

/// What one alarm says now: bad with its words, good, or unknown.
enum Reading {
    Bad(String),
    Good,
    Unknown,
}

impl Engine {
    /// Raises and resolves a vehicle's telemetry alarms from its latest
    /// readings.
    pub(crate) fn review_telemetry_alarms(&mut self, vehicle_id: &str) {
        let Some(vehicle) = self.vehicle(vehicle_id).cloned() else {
            return;
        };
        if !vehicle.armed {
            for kind in Kind::ALL {
                if let Some(watch) = self
                    .telemetry_alarms
                    .watches
                    .remove(&(vehicle.id.clone(), kind))
                    && watch.raised
                {
                    self.resolve(&kind.condition(&vehicle.id));
                }
            }
            return;
        }
        let now = self.clock.now_us();
        let readings: Vec<(Kind, Reading)> = Kind::ALL
            .into_iter()
            .map(|kind| (kind, self.alarm_reading(&vehicle, kind, now)))
            .collect();
        for (kind, reading) in readings {
            self.drive_alarm(&vehicle, kind, reading, now);
        }
    }

    fn drive_alarm(&mut self, vehicle: &Vehicle, kind: Kind, reading: Reading, now: u64) {
        let (raise_after, clear_after) = kind.timing();
        let condition = kind.condition(&vehicle.id);
        let watch = self
            .telemetry_alarms
            .watches
            .entry((vehicle.id.clone(), kind))
            .or_default();
        match reading {
            Reading::Unknown => {}
            Reading::Good => {
                watch.bad_since = None;
                let since = *watch.good_since.get_or_insert(now);
                if watch.raised
                    && clear_after.is_some_and(|after| now.saturating_sub(since) >= after)
                {
                    watch.raised = false;
                    self.resolve(&condition);
                    if kind == Kind::GpsLost {
                        self.gps_restored(vehicle);
                    }
                }
            }
            Reading::Bad(body) => {
                watch.good_since = None;
                let since = *watch.bad_since.get_or_insert(now);
                if watch.raised || now.saturating_sub(since) < raise_after {
                    return;
                }
                watch.raised = true;
                // Critical says more than low: one battery alarm at a time.
                if kind == Kind::BatteryCritical {
                    self.resolve(&Kind::BatteryLow.condition(&vehicle.id));
                }
                self.raise(Raise {
                    severity: kind.severity(),
                    kind: NotificationKind::Persistent,
                    subject: Subject::Vehicle {
                        id: vehicle.id.clone(),
                    },
                    title: format!("{}: {}", vehicle.name, title(kind)),
                    body: Some(body),
                    actions: Vec::new(),
                    condition: Some(condition),
                    report: None,
                });
            }
        }
    }

    /// The fix back after it was lost: said once, as the all clear, and
    /// replacing the last one.
    fn gps_restored(&mut self, vehicle: &Vehicle) {
        let condition = format!("telemetry:gps-restored:{}", vehicle.id);
        self.resolve(&condition);
        self.raise(Raise {
            severity: Severity::Info,
            kind: NotificationKind::Transient,
            subject: Subject::Vehicle {
                id: vehicle.id.clone(),
            },
            title: format!("{}: GPS restored", vehicle.name),
            body: Some("It has a 3D fix again.".into()),
            actions: Vec::new(),
            condition: Some(condition),
            report: None,
        });
    }

    fn alarm_active(&self, kind: Kind, vehicle_id: &str) -> bool {
        let condition = kind.condition(vehicle_id);
        self.state.notifications.values().any(|n| {
            n.condition.as_deref() == Some(condition.as_str())
                && n.state != NotificationState::Resolved
        })
    }

    fn fresh_item(&self, vehicle_id: &str, key: &str, now: u64) -> Option<&TelemetryItem> {
        self.telemetry(vehicle_id)?
            .items
            .iter()
            .filter(|i| i.key == key && now.saturating_sub(i.time_us) <= FRESH_US)
            .max_by_key(|i| i.time_us)
    }

    fn fresh_value(&self, vehicle_id: &str, key: &str, now: u64) -> Option<f64> {
        self.fresh_item(vehicle_id, key, now)?.value
    }

    fn alarm_reading(&mut self, vehicle: &Vehicle, kind: Kind, now: u64) -> Reading {
        let id = vehicle.id.as_str();
        match kind {
            Kind::GpsLost => {
                let Some(fix) = self.fresh_value(id, "gps.fix_type", now) else {
                    return Reading::Unknown;
                };
                if fix >= GPS_3D_FIX {
                    self.telemetry_alarms.had_fix.insert(id.to_owned());
                    return Reading::Good;
                }
                if !self.telemetry_alarms.had_fix.contains(id) {
                    return Reading::Unknown;
                }
                Reading::Bad(
                    "It has no 3D fix: holding position, missions and returning to launch depend on it. Be ready to fly it in a mode that needs no GPS, or land.".into(),
                )
            }
            Kind::GpsWeak => {
                let Some(fix) = self.fresh_value(id, "gps.fix_type", now) else {
                    return Reading::Unknown;
                };
                if fix < GPS_3D_FIX || self.alarm_active(Kind::GpsLost, id) {
                    return Reading::Good;
                }
                let satellites = self.fresh_value(id, "gps.satellites", now);
                let hdop = self.fresh_value(id, "gps.hdop", now);
                let few = satellites.is_some_and(|s| s < FEW_SATELLITES);
                let poor = hdop.is_some_and(|h| h > POOR_HDOP);
                if !few && !poor {
                    return Reading::Good;
                }
                let mut said = Vec::new();
                if let Some(s) = satellites {
                    said.push(format!("{s:.0} satellites"));
                }
                if let Some(h) = hdop {
                    said.push(format!("HDOP {h:.1}"));
                }
                Reading::Bad(format!("{}: its position may wander.", said.join(", ")))
            }
            Kind::BatteryLow | Kind::BatteryCritical => self.battery_reading(vehicle, kind, now),
            Kind::PositionBad => {
                let Some(text) = self
                    .fresh_item(id, "ekf.status", now)
                    .and_then(|i| i.text.clone())
                else {
                    return Reading::Unknown;
                };
                match text.as_str() {
                    "Good" => {
                        self.telemetry_alarms.had_position.insert(id.to_owned());
                        Reading::Good
                    }
                    "GPS glitching" | "No position" | "No attitude"
                        if self.telemetry_alarms.had_position.contains(id) =>
                    {
                        // Losing the fix says it first, and says why.
                        if text != "GPS glitching" && self.alarm_active(Kind::GpsLost, id) {
                            return Reading::Unknown;
                        }
                        Reading::Bad(format!(
                            "Its estimate says {}: modes that hold position may not hold.",
                            text.to_lowercase()
                        ))
                    }
                    _ => Reading::Unknown,
                }
            }
            Kind::Failsafe => {
                let Some(status) = self
                    .fresh_item(id, "system.status", now)
                    .and_then(|i| i.text.clone())
                else {
                    return Reading::Unknown;
                };
                if status != "Critical" && status != "Emergency" {
                    return Reading::Good;
                }
                Reading::Bad(vehicle.mode.as_ref().map_or_else(
                    || "The autopilot says a failsafe has acted.".to_owned(),
                    |mode| format!("The autopilot says a failsafe has acted. It is in {mode}."),
                ))
            }
            Kind::Fence => match self.fresh_value(id, "fence.breached", now) {
                Some(b) if b > 0.0 => {
                    Reading::Bad("It is outside its fence, so the fence's action applies.".into())
                }
                Some(_) => Reading::Good,
                None => Reading::Unknown,
            },
        }
    }

    /// The battery against the vehicle's own levels (ArduPilot's
    /// `BATT_LOW_VOLT` and `BATT_CRT_VOLT`, PX4's `BAT_LOW_THR` and
    /// `BAT_CRIT_THR`) and the operator's shares left, whichever comes first.
    fn battery_reading(&self, vehicle: &Vehicle, kind: Kind, now: u64) -> Reading {
        let param = |name: &str| {
            self.state
                .parameters
                .get(&parameter_id(&vehicle.id, name))
                .map(|p| p.value)
                .filter(|v| *v > 0.0)
        };
        let settings = &self.state.settings;
        let (volts_param, share_param, operator_pct) = match kind {
            Kind::BatteryCritical => (
                "BATT_CRT_VOLT",
                "BAT_CRIT_THR",
                settings.battery_critical_pct,
            ),
            _ => ("BATT_LOW_VOLT", "BAT_LOW_THR", settings.battery_low_pct),
        };
        let level_pct = [
            Some(f64::from(operator_pct)).filter(|p| *p > 0.0),
            param(share_param).map(|share| share * 100.0),
        ]
        .into_iter()
        .flatten()
        .reduce(f64::max);
        let level_v = param(volts_param);
        let remaining = self.fresh_value(&vehicle.id, "battery.remaining_pct", now);
        let volts = self.fresh_value(&vehicle.id, "battery.voltage_v", now);
        let by_share = remaining
            .zip(level_pct)
            .filter(|(left, level)| left <= level);
        let by_volts = volts.zip(level_v).filter(|(v, level)| v <= level);
        if let Some((left, _)) = by_share {
            let more = volts.map_or_else(String::new, |v| format!(", {v:.1} V"));
            return Reading::Bad(format!("{left:.0}% left{more}. {}", advice(kind)));
        }
        if let Some((v, level)) = by_volts {
            return Reading::Bad(format!(
                "{v:.1} V, at or below its own level of {level:.1} V. {}",
                advice(kind)
            ));
        }
        if (remaining.is_some() && level_pct.is_some()) || (volts.is_some() && level_v.is_some()) {
            Reading::Good
        } else {
            Reading::Unknown
        }
    }
}

const fn title(kind: Kind) -> &'static str {
    match kind {
        Kind::GpsLost => "GPS lost",
        Kind::GpsWeak => "GPS weak",
        Kind::BatteryLow => "Battery low",
        Kind::BatteryCritical => "Battery critical",
        Kind::PositionBad => "Position estimate bad",
        Kind::Failsafe => "Failsafe",
        Kind::Fence => "Geofence breached",
    }
}

const fn advice(kind: Kind) -> &'static str {
    match kind {
        Kind::BatteryCritical => "Land now.",
        _ => "Bring it back and land.",
    }
}

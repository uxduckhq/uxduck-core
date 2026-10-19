//! Coarse flight engagement, independent of the UI and its display timers.
//! Durations are observed vehicle-time, not evidence of manual piloting.

use std::collections::BTreeMap;

use uxduck_contracts::dto::analytics::{
    AnalyticsVehicleKind, DaemonEvent, FlightTimeDelta, FlightTimeUsage,
};
use uxduck_contracts::dto::initialize::RunsIn;
use uxduck_core::domain::link::LinkKind;
use uxduck_core::domain::telemetry::off_the_ground;
use uxduck_core::domain::vehicle::Presence;

use crate::Engine;

const FRESH_US: u64 = 3_000_000;
const REPORT_US: u64 = 30_000_000;

#[derive(Debug)]
struct Sample {
    at: u64,
    kind: AnalyticsVehicleKind,
    armed_until: u64,
    air_until: u64,
}

#[derive(Debug, Default)]
struct Total {
    armed_us: u64,
    air_us: u64,
}

#[derive(Debug, Default)]
pub(crate) struct FlightUsage {
    seen: BTreeMap<String, Sample>,
    pending: BTreeMap<AnalyticsVehicleKind, Total>,
    reported_at: Option<u64>,
}

impl FlightUsage {
    fn observe(&mut self, id: &str, sample: Sample, armed: bool) {
        if let Some(before) = self.seen.remove(id)
            && before.kind == sample.kind
        {
            let total = self.pending.entry(before.kind).or_default();
            total.armed_us += sample.at.min(before.armed_until).saturating_sub(before.at);
            total.air_us += sample.at.min(before.air_until).saturating_sub(before.at);
        }
        self.reported_at.get_or_insert(sample.at);
        if armed {
            self.seen.insert(id.to_owned(), sample);
        }
    }

    fn take(&mut self, now: u64) -> FlightTimeUsage {
        self.reported_at = Some(now);
        // Keep fractional milliseconds rather than dropping them at high rates.
        let reports = self
            .pending
            .iter_mut()
            .filter_map(|(&vehicle_kind, total)| {
                let armed_ms = total.armed_us / 1000;
                let airborne_ms = (total.air_us / 1000).min(armed_ms);
                total.armed_us -= armed_ms * 1000;
                total.air_us -= airborne_ms * 1000;
                (armed_ms > 0).then_some(FlightTimeDelta {
                    vehicle_kind,
                    armed_ms,
                    airborne_ms,
                })
            })
            .collect();
        FlightTimeUsage { reports }
    }
}

/// A managed simulator wins. Hardware-backed USB serial plus a reported
/// nonzero autopilot UID is hardware evidence; network/virtual serial is unknown.
fn kind(link: Option<&LinkKind>, uid: Option<&str>) -> AnalyticsVehicleKind {
    match link {
        Some(LinkKind::Simulator { .. }) => AnalyticsVehicleKind::Simulated,
        Some(LinkKind::Serial { usb: Some(usb), .. })
            if usb.vendor_id != 0
                && usb.product_id != 0
                && uid.is_some_and(|id| {
                    id.len() >= 16
                        && id.bytes().all(|c| c.is_ascii_hexdigit())
                        && id.bytes().any(|c| c != b'0')
                        && id.bytes().any(|c| !matches!(c, b'f' | b'F'))
                }) =>
        {
            AnalyticsVehicleKind::Real
        }
        _ => AnalyticsVehicleKind::Unknown,
    }
}

impl Engine {
    pub(crate) fn analytics_vehicle_kind(&self, vehicle_id: &str) -> AnalyticsVehicleKind {
        self.vehicle(vehicle_id)
            .map_or(AnalyticsVehicleKind::Unknown, |vehicle| {
                kind(
                    self.state.links.get(&vehicle.link_id).map(|l| &l.kind),
                    vehicle.uid.as_deref(),
                )
            })
    }

    /// Called only for source telemetry and vehicle-state transitions, never
    /// recursively for UXDuck's derived flight-time display values.
    pub(crate) fn observe_flight_usage(&mut self, vehicle_id: &str) {
        if !self.state.settings.usage_analytics {
            self.flight_usage = FlightUsage::default();
            return;
        }
        let Some(vehicle) = self.vehicle(vehicle_id) else {
            return;
        };
        let now = self.clock.now_us();
        let id = vehicle.id.clone();
        let vehicle_kind = kind(
            self.state.links.get(&vehicle.link_id).map(|l| &l.kind),
            vehicle.uid.as_deref(),
        );
        let armed = vehicle.armed && vehicle.presence == Presence::Online;
        let ended = !armed && self.flight_usage.seen.contains_key(&id);
        let heartbeat = self
            .system
            .vehicle_heartbeat_us(std::slice::from_ref(&id))
            .unwrap_or(vehicle.last_heartbeat_us);
        let armed_until = if armed {
            heartbeat
                .saturating_add(FRESH_US)
                .min(now.saturating_add(FRESH_US))
        } else {
            now
        };
        let mut air_until = now;
        if armed && super::flight_time::flies(&vehicle.vehicle_type) {
            let previous_up = self
                .flight_usage
                .seen
                .get(&id)
                .is_some_and(|s| s.air_until > s.at);
            if let Some(telemetry) = self.telemetry(&id) {
                // Prefer a fresh autopilot landed-state. Fall back only to
                // fresh fused height, using the display's existing hysteresis.
                let state = telemetry
                    .items
                    .iter()
                    .find(|i| i.key == "flight.state" && i.time_us.saturating_add(FRESH_US) > now)
                    .and_then(|i| off_the_ground(i.text.as_deref()?).map(|up| (up, i.time_us)));
                let evidence = state.or_else(|| {
                    telemetry
                        .positions
                        .iter()
                        .filter(|p| p.source == "fused" && p.time_us.saturating_add(FRESH_US) > now)
                        .find_map(|p| {
                            p.altitudes
                                .iter()
                                .find(|a| {
                                    a.frame
                                        == uxduck_core::domain::telemetry::AltitudeFrame::AboveHome
                                        && a.meters.is_finite()
                                })
                                .map(|a| {
                                    (
                                        if a.meters
                                            > uxduck_core::domain::command::FLYING_ABOVE_HOME_M
                                        {
                                            true
                                        } else if a.meters < 1.0 {
                                            false
                                        } else {
                                            previous_up
                                        },
                                        p.time_us,
                                    )
                                })
                        })
                });
                if let Some((true, at)) = evidence {
                    air_until = at.saturating_add(FRESH_US).min(armed_until);
                }
            }
        }
        self.flight_usage.observe(
            &id,
            Sample {
                at: now,
                kind: vehicle_kind,
                armed_until,
                air_until,
            },
            armed,
        );
        if self.config.capabilities.runs_in != RunsIn::Browser
            && (ended
                || self
                    .flight_usage
                    .reported_at
                    .is_some_and(|at| now.saturating_sub(at) >= REPORT_US))
        {
            let reports = self.flight_usage.take(now);
            for delta in reports.reports {
                self.track(DaemonEvent::VehicleFlightTime { delta });
            }
        }
    }

    /// One browser daemon, one set of counters: multiple windows never
    /// independently count the same vehicle. Installed daemons send directly.
    pub(crate) fn take_flight_usage(&mut self) -> FlightTimeUsage {
        if self.config.capabilities.runs_in != RunsIn::Browser
            || !self.state.settings.usage_analytics
        {
            return FlightTimeUsage::default();
        }
        self.flight_usage.take(self.clock.now_us())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(at: u64, up: bool) -> Sample {
        Sample {
            at,
            kind: AnalyticsVehicleKind::Simulated,
            armed_until: at + FRESH_US,
            air_until: if up { at + FRESH_US } else { at },
        }
    }

    #[test]
    fn deltas_are_consumed_once_and_only_airborne_time_counts_as_flight() {
        let mut usage = FlightUsage::default();
        for sec in 0..=40 {
            usage.observe(
                "one",
                sample(sec * 1_000_000, (10..30).contains(&sec)),
                sec < 40,
            );
        }
        let reports = usage.take(40_000_000).reports;
        assert_eq!(
            reports,
            vec![FlightTimeDelta {
                vehicle_kind: AnalyticsVehicleKind::Simulated,
                armed_ms: 40_000,
                airborne_ms: 20_000
            }]
        );
        assert!(usage.take(40_000_000).reports.is_empty());
    }

    #[test]
    fn telemetry_gaps_are_capped_and_reclassification_does_not_relabel_old_time() {
        let mut usage = FlightUsage::default();
        usage.observe("one", sample(0, true), true);
        usage.observe("one", sample(60_000_000, true), true);
        let mut changed = sample(61_000_000, true);
        changed.kind = AnalyticsVehicleKind::Unknown;
        usage.observe("one", changed, true);
        assert_eq!(
            usage.take(61_000_000).reports,
            vec![FlightTimeDelta {
                vehicle_kind: AnalyticsVehicleKind::Simulated,
                armed_ms: 3_000,
                airborne_ms: 3_000
            }]
        );
    }

    #[test]
    fn fractional_carry_never_reports_more_airborne_than_armed() {
        let mut usage = FlightUsage::default();
        usage.pending.insert(
            AnalyticsVehicleKind::Unknown,
            Total {
                armed_us: 1_100,
                air_us: 900,
            },
        );
        assert_eq!(usage.take(0).reports[0].airborne_ms, 0);
        let total = usage
            .pending
            .get_mut(&AnalyticsVehicleKind::Unknown)
            .unwrap();
        total.armed_us += 200;
        total.air_us += 200;
        assert!(usage.take(1).reports.is_empty());
        usage
            .pending
            .get_mut(&AnalyticsVehicleKind::Unknown)
            .unwrap()
            .armed_us += 700;
        let delta = &usage.take(2).reports[0];
        assert_eq!((delta.armed_ms, delta.airborne_ms), (1, 1));
    }

    #[test]
    fn simultaneous_vehicles_add_vehicle_time_without_vehicle_ids() {
        let mut usage = FlightUsage::default();
        for sec in 0..=5 {
            for id in ["one", "two"] {
                usage.observe(id, sample(sec * 1_000_000, true), true);
            }
        }
        assert_eq!(usage.take(5_000_000).reports[0].airborne_ms, 10_000);
    }

    #[test]
    fn only_managed_simulators_and_hardware_backed_serial_are_classified() {
        let simulator: LinkKind = serde_json::from_value(serde_json::json!({"type":"simulator","vehicle":"multicopter","latitudeDeg":0.0,"longitudeDeg":0.0,"altitudeM":0.0,"headingDeg":0.0})).unwrap();
        let serial = LinkKind::Serial {
            device: "ignored".into(),
            baud: 115_200,
            usb: Some(uxduck_core::domain::link::UsbId {
                vendor_id: 1,
                product_id: 2,
                serial_number: None,
            }),
        };
        let network = LinkKind::UdpListen {
            host: "127.0.0.1".into(),
            port: 14550,
        };
        assert_eq!(
            kind(Some(&simulator), Some("abcd")),
            AnalyticsVehicleKind::Simulated
        );
        assert_eq!(
            kind(Some(&serial), Some("00000000")),
            AnalyticsVehicleKind::Unknown
        );
        assert_eq!(kind(Some(&serial), None), AnalyticsVehicleKind::Unknown);
        assert_eq!(
            kind(Some(&serial), Some("000000001234abcd")),
            AnalyticsVehicleKind::Real
        );
        assert_eq!(
            kind(Some(&network), Some("1234abcd")),
            AnalyticsVehicleKind::Unknown
        );
        assert_eq!(kind(None, Some("1234abcd")), AnalyticsVehicleKind::Unknown);
    }
}

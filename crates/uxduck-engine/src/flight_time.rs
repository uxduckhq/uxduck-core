//! Flight time (S103, maintainer 2026-10-08): how long each vehicle has
//! been armed, and for one that flies, how long it has been in the air
//! since it armed. Worked out here, not in the page, so a reload or a
//! second window shows the same and the agent and plugins read it too.
//!
//! - `flight.armed_s`, "Time armed": from arming, as UXDuck first saw it
//!   armed. After a disarm it holds the flight's time until the next arm.
//! - `flight.air_s`, "Time in the air": planes, VTOLs and multicopters
//!   only, the time off the ground since arming, added up over hops. Off
//!   the ground is what the autopilot says (`flight.state`, from
//!   `EXTENDED_SYS_STATE`) when it says it; otherwise above
//!   [`AIR_ABOVE_M`] above home, and back down below [`GROUND_BELOW_M`].
//!
//! Sent with the vehicle's telemetry, at most once a second each, never
//! journaled; a daemon start forgets them.
//!
//! Each arm to disarm is also kept as a [`Flight`] (C8), journaled when it
//! begins and when it ends, with its highest point, top speed, modes,
//! battery and who acted on the vehicle; one left open by a crash is ended
//! at the next start, where it was last heard.

use std::collections::BTreeMap;

use uxduck_core::domain::command::FLYING_ABOVE_HOME_M;
use uxduck_core::domain::telemetry::{TelemetryItem, Unit, VehicleTelemetry, off_the_ground};
use uxduck_core::domain::telemetry_layout::VehicleClass;

use uxduck_core::domain::flight::{Flight, FlightEnd, FlightOperator};
use uxduck_core::events::{Actor, Change};

use crate::Engine;

/// Off the ground above this height above home, with no word from the autopilot.
const AIR_ABOVE_M: f64 = FLYING_ABOVE_HOME_M;
/// Back on the ground below this, so a hover at the line does not flicker.
const GROUND_BELOW_M: f64 = 1.0;

const SOURCE: &str = "flight";

/// Each vehicle's flight time, by vehicle id.
#[derive(Debug, Default)]
pub(crate) struct FlightTimes(BTreeMap<String, Timer>);

#[derive(Debug, Default)]
struct Timer {
    /// While armed: when.
    armed_at_us: Option<u64>,
    /// After a disarm: how long it was armed.
    armed_us: u64,
    /// While in the air: since when.
    air_since_us: Option<u64>,
    /// Time in the air banked before the current hop.
    air_us: u64,
    /// Whole seconds last sent, so a second is sent once.
    sent: Option<(u64, u64)>,
    /// The flight this arming began (C8), and what it has seen so far.
    flight: Option<String>,
    max_height_m: Option<f64>,
    max_speed_mps: Option<f64>,
    modes: Vec<String>,
    /// The flight ended when contact was lost while armed, to open again
    /// if the vehicle is heard still armed.
    lost: Option<String>,
}

impl Timer {
    fn armed_us(&self, now_us: u64) -> u64 {
        self.armed_at_us
            .map_or(self.armed_us, |at| now_us.saturating_sub(at))
    }

    fn air_us(&self, now_us: u64) -> u64 {
        self.air_us + self.air_since_us.map_or(0, |at| now_us.saturating_sub(at))
    }
}

/// Whether vehicles of this `MAV_TYPE` fly.
pub(crate) fn flies(vehicle_type: &str) -> bool {
    matches!(
        VehicleClass::of(vehicle_type),
        VehicleClass::Plane | VehicleClass::Copter | VehicleClass::Airship
    )
}

#[allow(
    clippy::cast_precision_loss,
    reason = "microseconds of one flight fit an f64 exactly"
)]
fn seconds(us: u64) -> f64 {
    us as f64 / 1e6
}

fn flight_actor() -> Actor {
    Actor {
        client_id: "uxduck-flight".into(),
        operator_id: None,
        identity: None,
    }
}

/// A flight in words for the vehicle's history: "Flew 12 min, highest
/// 45 m", or "Armed 3 min" for one that stayed on the ground.
fn flight_words(flight: &Flight) -> String {
    let span = |us: u64| match us / 60_000_000 {
        0 => format!("{} s", us / 1_000_000),
        m => format!("{m} min"),
    };
    let armed = flight
        .ended_at_us
        .unwrap_or(flight.armed_at_us)
        .saturating_sub(flight.armed_at_us);
    let mut text = match (flight.end == FlightEnd::ContactLost, flight.air_us > 0) {
        (false, true) => format!("Flew {}", span(flight.air_us)),
        (false, false) => format!("Armed {}", span(armed)),
        (true, true) => format!("Lost contact after flying {}", span(flight.air_us)),
        (true, false) => format!("Lost contact after {} armed", span(armed)),
    };
    if let Some(height) = flight.max_height_m.filter(|h| *h >= 1.0) {
        text = format!("{text}, highest {height:.0} m");
    }
    if flight.simulated {
        text.push_str(", simulated");
    }
    text
}

/// The condition a flight's "Save to your account" notice goes with.
fn keep_condition(flight_id: &str) -> String {
    format!("flight-keep:{flight_id}")
}

fn higher(was: Option<f64>, now: Option<f64>) -> Option<f64> {
    match (was, now) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

impl Engine {
    /// A vehicle armed: its times start again, and a flight begins.
    pub(crate) fn flight_armed(&mut self, vehicle_id: &str) {
        let now = self.clock.now_us();
        let id = self
            .vehicle(vehicle_id)
            .map_or_else(|| vehicle_id.to_owned(), |v| v.id.clone());
        self.flight_times.0.insert(
            id.clone(),
            Timer {
                armed_at_us: Some(now),
                ..Timer::default()
            },
        );
        self.begin_flight(&id, now, false);
        self.review_flight_time(vehicle_id);
    }

    fn telemetry_value(&self, vehicle_id: &str, key: &str) -> Option<f64> {
        self.telemetry(vehicle_id)
            .and_then(|t| t.items.iter().find(|i| i.key == key))
            .and_then(|i| i.value)
    }

    /// A flight begins (C8), journaled now so a crash cannot lose it.
    fn begin_flight(&mut self, vehicle_id: &str, armed_at_us: u64, seen_armed: bool) {
        let Some(vehicle) = self.vehicle(vehicle_id).cloned() else {
            return;
        };
        let simulated = self
            .state
            .links
            .get(&vehicle.link_id)
            .is_some_and(|l| l.kind.is_simulator());
        let flight = Flight {
            id: format!("fl{}", &self.random.token()[..16]),
            vehicle_id: vehicle.id.clone(),
            vehicle_name: vehicle.name.clone(),
            vehicle_type: vehicle.vehicle_type.clone(),
            simulated,
            seen_armed,
            armed_at_us,
            ended_at_us: None,
            end: FlightEnd::Open,
            air_us: 0,
            max_height_m: None,
            max_speed_mps: None,
            modes: vehicle.mode.iter().cloned().collect(),
            battery_start_pct: self.telemetry_value(&vehicle.id, "battery.remaining_pct"),
            battery_end_pct: None,
            operators: Vec::new(),
            keep: false,
            params: crate::param_definitions::baselines::at_arm(self, &vehicle.id),
            version: 1,
        };
        if let Some(timer) = self.flight_times.0.get_mut(&vehicle.id) {
            timer.flight = Some(flight.id.clone());
            timer.modes.clone_from(&flight.modes);
        }
        self.commit_as(flight_actor(), Change::FlightUpserted { flight });
    }

    /// Who acted on the vehicle since `from_us`: commands they sent and the
    /// controllers they hold on it, each from their first, in order.
    fn flight_operators(&self, vehicle_id: &str, from_us: u64) -> Vec<FlightOperator> {
        let mut first: BTreeMap<String, u64> = BTreeMap::new();
        for receipt in self.state.receipts.values() {
            if receipt.vehicle_id == vehicle_id
                && receipt.created_at_us >= from_us
                && let Some(op) = &receipt.operator_id
            {
                let at = first.entry(op.clone()).or_insert(receipt.created_at_us);
                *at = (*at).min(receipt.created_at_us);
            }
        }
        for controller in self.state.controllers.values() {
            if controller.target.as_deref() == Some(vehicle_id)
                && let Some(op) = controller
                    .claim
                    .as_ref()
                    .and_then(|c| c.operator_id.clone())
            {
                first.entry(op).or_insert(from_us);
            }
        }
        let mut operators: Vec<FlightOperator> = first
            .into_iter()
            .map(|(operator_id, from_us)| FlightOperator {
                name: self
                    .state
                    .operators
                    .get(&operator_id)
                    .map_or_else(|| "Someone".to_owned(), |o| o.name.clone()),
                operator_id,
                from_us,
            })
            .collect();
        operators.sort_by_key(|o| o.from_us);
        operators
    }

    /// A flight ends (C8): its numbers and who flew it, journaled.
    fn end_flight(&mut self, vehicle_id: &str, end: FlightEnd, at_us: u64) {
        let Some(timer) = self.flight_times.0.get_mut(vehicle_id) else {
            return;
        };
        let Some(flight_id) = timer.flight.take() else {
            return;
        };
        let air_us = timer.air_us(at_us);
        let (height, speed, modes) = (
            timer.max_height_m,
            timer.max_speed_mps,
            std::mem::take(&mut timer.modes),
        );
        let Some(mut flight) = self.state.flights.get(&flight_id).cloned() else {
            return;
        };
        flight.ended_at_us = Some(at_us);
        flight.end = end;
        flight.air_us = air_us;
        flight.max_height_m = height;
        flight.max_speed_mps = speed;
        flight.modes = modes;
        flight.battery_end_pct = self.telemetry_value(vehicle_id, "battery.remaining_pct");
        flight.operators = self.flight_operators(vehicle_id, flight.armed_at_us);
        // "Save every flight to my account" (C9): kept as it ends, not suggested.
        flight.keep |= self.state.settings.save_flights && !flight.simulated;
        flight.version += 1;
        self.commit_as(
            flight_actor(),
            Change::FlightUpserted {
                flight: flight.clone(),
            },
        );
        self.record(
            vehicle_id,
            uxduck_core::domain::history::HistoryEvent::FlightEnded {
                flight_id: flight.id.clone(),
                text: flight_words(&flight),
            },
        );
        self.suggest_keeping(&flight);
    }

    /// `flight/list`: the newest flights, and totals over all of them (C9).
    pub(crate) fn list_flights(
        &self,
        params: &uxduck_contracts::dto::flight::FlightListParams,
    ) -> uxduck_contracts::dto::flight::FlightListResult {
        let limit = usize::try_from(params.limit.unwrap_or(100).clamp(1, 500)).unwrap_or(100);
        let vehicle = params.vehicle_id.as_deref().map(|id| {
            self.vehicle(id)
                .map_or_else(|| id.to_owned(), |v| v.id.clone())
        });
        let mut flights: Vec<_> = self
            .state
            .flights
            .values()
            .filter(|f| vehicle.as_ref().is_none_or(|v| &f.vehicle_id == v))
            .collect();
        flights.sort_by_key(|f| std::cmp::Reverse(f.armed_at_us));
        let mut totals = uxduck_contracts::dto::flight::FlightTotals {
            count: u32::try_from(flights.len()).unwrap_or(u32::MAX),
            ..Default::default()
        };
        let since = params.totals_since_us;
        for flight in &flights {
            let recent = since.is_some_and(|s| flight.armed_at_us >= s);
            let (all, lately) = if flight.simulated {
                (
                    &mut totals.simulated_air_us,
                    &mut totals.simulated_air_us_since,
                )
            } else {
                (&mut totals.real_air_us, &mut totals.real_air_us_since)
            };
            *all += flight.air_us;
            if recent {
                *lately += flight.air_us;
            }
        }
        uxduck_contracts::dto::flight::FlightListResult {
            flights: flights.into_iter().take(limit).map(Into::into).collect(),
            totals,
        }
    }

    /// Saves a flight to the account, or keeps it here only (C9).
    pub(crate) fn keep_flight(
        &mut self,
        connection: crate::ConnectionId,
        id: &str,
        keep: bool,
        always: bool,
    ) -> Result<uxduck_contracts::dto::flight::Flight, uxduck_contracts::errors::ApiError> {
        let mut flight = self.state.flights.get(id).cloned().ok_or_else(|| {
            uxduck_contracts::errors::ApiError::new(
                uxduck_contracts::errors::ErrorKind::NotFound,
                format!("no flight {id}"),
            )
        })?;
        if flight.keep != keep {
            flight.keep = keep;
            flight.version += 1;
            self.commit(
                connection,
                Change::FlightUpserted {
                    flight: flight.clone(),
                },
            );
        }
        if always && self.state.settings.save_flights != keep {
            let mut settings = self.state.settings.clone();
            settings.save_flights = keep;
            settings.version += 1;
            self.commit(connection, Change::SettingsChanged { settings });
        }
        self.resolve(&keep_condition(id));
        Ok((&flight).into())
    }

    /// A real flight just finished: suggested for the account, never sent
    /// unasked (maintainer, 2026-10-23: flight data off by default).
    fn suggest_keeping(&mut self, flight: &Flight) {
        if flight.simulated || flight.keep || flight.end != FlightEnd::Disarmed {
            return;
        }
        let minutes = flight.air_us / 60_000_000;
        let air = if minutes > 0 {
            format!("{minutes} min in the air")
        } else {
            format!("{} s in the air", flight.air_us / 1_000_000)
        };
        self.raise(crate::commands::Raise {
            severity: uxduck_core::domain::notification::Severity::Info,
            kind: uxduck_core::domain::notification::NotificationKind::Persistent,
            subject: uxduck_core::domain::notification::Subject::Vehicle {
                id: flight.vehicle_id.clone(),
            },
            title: format!("{} finished a flight", flight.vehicle_name),
            body: Some(format!(
                "{air}. Save it to your account to have it in your logbook on every computer."
            )),
            actions: vec![
                uxduck_core::domain::notification::NotificationAction {
                    label: "Save to your account".into(),
                    action: uxduck_core::domain::notification::Action::KeepFlight {
                        flight_id: flight.id.clone(),
                        always: false,
                    },
                },
                uxduck_core::domain::notification::NotificationAction {
                    label: "Always save".into(),
                    action: uxduck_core::domain::notification::Action::KeepFlight {
                        flight_id: flight.id.clone(),
                        always: true,
                    },
                },
            ],
            condition: Some(keep_condition(&flight.id)),
            report: None,
        });
    }

    /// Flights a crash left open, ended where their vehicle was last heard.
    pub(crate) fn close_interrupted_flights(&mut self) {
        let open: Vec<Flight> = self
            .state
            .flights
            .values()
            .filter(|f| f.end == FlightEnd::Open)
            .cloned()
            .collect();
        for mut flight in open {
            let heard = self
                .state
                .vehicles
                .get(&flight.vehicle_id)
                .map_or(flight.armed_at_us, |v| v.last_heartbeat_us);
            flight.ended_at_us = Some(heard.max(flight.armed_at_us));
            flight.end = FlightEnd::Interrupted;
            flight.version += 1;
            self.commit_as(flight_actor(), Change::FlightUpserted { flight });
        }
    }

    /// A vehicle went offline or was heard again (C8): an armed one's flight
    /// ends as contact lost when UXDuck last heard it, and opens again if it
    /// comes back still armed. One that comes back disarmed stays ended.
    pub(crate) fn flight_presence(&mut self, vehicle_id: &str) {
        let Some(vehicle) = self.vehicle(vehicle_id) else {
            return;
        };
        let (id, armed, presence, heard) = (
            vehicle.id.clone(),
            vehicle.armed,
            vehicle.presence,
            vehicle.last_heartbeat_us,
        );
        let Some(timer) = self.flight_times.0.get(&id) else {
            return;
        };
        match presence {
            uxduck_core::domain::vehicle::Presence::Offline if timer.flight.is_some() => {
                let flight_id = timer.flight.clone();
                self.end_flight(&id, FlightEnd::ContactLost, heard);
                if let Some(timer) = self.flight_times.0.get_mut(&id) {
                    timer.lost = flight_id;
                }
            }
            uxduck_core::domain::vehicle::Presence::Online => {
                let Some(timer) = self.flight_times.0.get_mut(&id) else {
                    return;
                };
                let Some(flight_id) = timer.lost.take() else {
                    return;
                };
                if !armed {
                    return;
                }
                let Some(mut flight) = self.state.flights.get(&flight_id).cloned() else {
                    return;
                };
                timer.flight = Some(flight_id);
                timer.max_height_m = flight.max_height_m;
                timer.max_speed_mps = flight.max_speed_mps;
                timer.modes.clone_from(&flight.modes);
                flight.end = FlightEnd::Open;
                flight.ended_at_us = None;
                flight.version += 1;
                self.commit_as(flight_actor(), Change::FlightUpserted { flight });
            }
            _ => {}
        }
    }

    /// A vehicle disarmed: its times stop and hold, and its flight ends.
    pub(crate) fn flight_disarmed(&mut self, vehicle_id: &str) {
        let now = self.clock.now_us();
        let id = self
            .vehicle(vehicle_id)
            .map_or_else(|| vehicle_id.to_owned(), |v| v.id.clone());
        self.end_flight(&id, FlightEnd::Disarmed, now);
        if let Some(timer) = self.flight_times.0.get_mut(vehicle_id) {
            if let Some(at) = timer.armed_at_us.take() {
                timer.armed_us = now.saturating_sub(at);
            }
            if let Some(at) = timer.air_since_us.take() {
                timer.air_us += now.saturating_sub(at);
            }
            timer.sent = None;
        }
        self.review_flight_time(vehicle_id);
    }

    /// Whether a vehicle is off the ground now: the autopilot's word, else
    /// its height above home, with `was` for the band between the two lines.
    pub(crate) fn off_the_ground(&self, vehicle_id: &str, was: bool) -> bool {
        let said = self
            .telemetry(vehicle_id)
            .and_then(|t| t.items.iter().find(|i| i.key == "flight.state"))
            .and_then(|i| i.text.as_deref())
            .and_then(off_the_ground);
        if let Some(said) = said {
            return said;
        }
        match self.height_above_home(vehicle_id) {
            Some(h) if h > AIR_ABOVE_M => true,
            Some(h) if h < GROUND_BELOW_M => false,
            Some(_) => was,
            None => false,
        }
    }

    /// Telemetry arrived, or the vehicle armed or disarmed: start or stop
    /// its time in the air, and send its times when a second has passed.
    pub(crate) fn review_flight_time(&mut self, vehicle_id: &str) {
        let Some(vehicle) = self.vehicle(vehicle_id) else {
            return;
        };
        let (id, armed, aerial) = (
            vehicle.id.clone(),
            vehicle.armed,
            flies(&vehicle.vehicle_type),
        );
        let now = self.clock.now_us();
        // Armed when UXDuck first heard it: from then, a flight seen already armed.
        if armed
            && self
                .flight_times
                .0
                .get(&id)
                .is_none_or(|t| t.armed_at_us.is_none())
        {
            self.flight_times.0.insert(
                id.clone(),
                Timer {
                    armed_at_us: Some(now),
                    ..Timer::default()
                },
            );
            self.begin_flight(&id, now, true);
        }
        // What the flight has seen: its highest point, top speed and modes.
        if armed {
            let height = self.height_above_home(&id);
            let speed = self.telemetry_value(&id, "speed.ground_mps");
            let mode = self.vehicle(&id).and_then(|v| v.mode.clone());
            if let Some(timer) = self.flight_times.0.get_mut(&id)
                && timer.flight.is_some()
            {
                timer.max_height_m = higher(timer.max_height_m, height);
                timer.max_speed_mps = higher(timer.max_speed_mps, speed);
                if let Some(mode) = mode
                    && timer.modes.last() != Some(&mode)
                    && !timer.modes.contains(&mode)
                {
                    timer.modes.push(mode);
                }
            }
        }
        let flying = self
            .flight_times
            .0
            .get(&id)
            .is_some_and(|t| t.air_since_us.is_some());
        let up = armed && aerial && self.off_the_ground(&id, flying);
        let Some(timer) = self.flight_times.0.get_mut(&id) else {
            return;
        };
        match (up, timer.air_since_us) {
            (true, None) => timer.air_since_us = Some(now),
            (false, Some(at)) => {
                timer.air_us += now.saturating_sub(at);
                timer.air_since_us = None;
            }
            _ => {}
        }
        let (armed_us, air_us) = (timer.armed_us(now), timer.air_us(now));
        let whole = (armed_us / 1_000_000, air_us / 1_000_000);
        if timer.sent == Some(whole) {
            return;
        }
        // Noted before sending: sending reviews again, and must stop there.
        timer.sent = Some(whole);
        let item = |key: &str, label: &str, us: u64| TelemetryItem {
            key: key.into(),
            label: label.into(),
            unit: Unit::Second,
            value: Some(seconds(us)),
            text: None,
            source: SOURCE.into(),
            component_id: 0,
            time_us: now,
        };
        let mut items = vec![item("flight.armed_s", "Time armed", armed_us)];
        if aerial {
            items.push(item("flight.air_s", "Time in the air", air_us));
        }
        self.telemetry_observed(VehicleTelemetry {
            vehicle_id: id,
            positions: Vec::new(),
            items,
        });
    }
}

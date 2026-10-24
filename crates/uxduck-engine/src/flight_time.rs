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

use uxduck_core::domain::flight::{Flight, FlightEnd, FlightMission, FlightOperator};
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
    /// The vehicle flew a mission this flight, and said it was complete.
    mission_flown: bool,
    mission_complete: bool,
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
            recorded_here: true,
            left_account: false,
            contact_lost_at_us: None,
            mission: None,
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
        let (flown, complete) = (
            std::mem::take(&mut timer.mission_flown),
            std::mem::take(&mut timer.mission_complete),
        );
        let Some(mut flight) = self.state.flights.get(&flight_id).cloned() else {
            return;
        };
        flight.ended_at_us = Some(at_us);
        flight.end = end;
        flight.contact_lost_at_us = None;
        flight.air_us = air_us;
        flight.max_height_m = height;
        flight.max_speed_mps = speed;
        flight.modes = modes;
        flight.battery_end_pct = self.telemetry_value(vehicle_id, "battery.remaining_pct");
        flight.operators = self.flight_operators(vehicle_id, flight.armed_at_us);
        flight.mission = flown.then(|| self.flight_mission(&flight, complete));
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
        use uxduck_contracts::errors::{ApiError, ErrorKind};
        let mut flight = self
            .state
            .flights
            .get(id)
            .cloned()
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no flight {id}")))?;
        if keep && flight.left_account {
            // Saved again after a delete (2026-10-24): never by itself, only
            // when asked, and under a new id the account has no removal for.
            let again = Flight {
                id: format!("fl{}", &self.random.token()[..16]),
                keep: true,
                left_account: false,
                version: 1,
                ..flight
            };
            self.commit(
                connection,
                Change::FlightReplaced {
                    old_id: id.to_owned(),
                    flight: again.clone(),
                },
            );
            self.resolve(&keep_condition(id));
            return Ok((&again).into());
        }
        if keep && !flight.keep {
            flight.keep = true;
            flight.version += 1;
            self.commit(
                connection,
                Change::FlightUpserted {
                    flight: flight.clone(),
                },
            );
            // A finished mission's other flights go with it.
            let others: Vec<Flight> = flight
                .mission
                .as_ref()
                .map(|m| {
                    self.run_flights(&m.run_id)
                        .into_iter()
                        .filter(|f| f.id != flight.id && !f.keep && !f.left_account)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            for mut other in others {
                other.keep = true;
                other.version += 1;
                self.commit(connection, Change::FlightUpserted { flight: other });
            }
        } else if !keep && flight.keep {
            // Deleted from the account (2026-10-24): there is no "unsave".
            self.commit(connection, Change::FlightRemoved { id: id.to_owned() });
        }
        if always && self.state.settings.save_flights != keep {
            let mut settings = self.state.settings.clone();
            settings.save_flights = keep;
            settings.version += 1;
            self.commit(connection, Change::SettingsChanged { settings });
        }
        self.resolve(&keep_condition(id));
        // Gone from here too when it only came from the account.
        let after = self.state.flights.get(id).cloned().unwrap_or(Flight {
            keep: false,
            ..flight
        });
        Ok((&after).into())
    }

    /// A real flight just finished: suggested for the account, never sent
    /// unasked (maintainer, 2026-10-23: flight data off by default).
    fn suggest_keeping(&mut self, flight: &Flight) {
        if flight.simulated || flight.keep || flight.end != FlightEnd::Disarmed {
            return;
        }
        let span = |us: u64| match us / 60_000_000 {
            0 => format!("{} s", us / 1_000_000),
            m => format!("{m} min"),
        };
        // A mission is asked about once it is finished, for all its flights;
        // landing mid-mission (a battery swap) asks nothing (C8, 2026-10-24).
        let (title, air) = match &flight.mission {
            Some(mission) if !mission.completed => return,
            Some(mission) => {
                let run = self.run_flights(&mission.run_id);
                let air: u64 = run.iter().map(|f| f.air_us).sum();
                let what = mission
                    .plan_name
                    .as_deref()
                    .map_or_else(String::new, |n| format!("{n} · "));
                let flights = match run.len() {
                    1 => String::new(),
                    n => format!("{n} flights, "),
                };
                (
                    format!("{} finished its mission", flight.vehicle_name),
                    format!("{what}{flights}{} in the air", span(air)),
                )
            }
            None => (
                format!("{} finished a flight", flight.vehicle_name),
                format!("{} in the air", span(flight.air_us)),
            ),
        };
        self.raise(crate::commands::Raise {
            severity: uxduck_core::domain::notification::Severity::Info,
            kind: uxduck_core::domain::notification::NotificationKind::Persistent,
            subject: uxduck_core::domain::notification::Subject::Vehicle {
                id: flight.vehicle_id.clone(),
            },
            title,
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

    /// The mission a flight just flew: the one UXDuck last knew on the
    /// vehicle, by its content (S134), and the plan it came from when UXDuck
    /// uploaded it. The run goes on from the vehicle's last flight only when
    /// that one landed mid-mission with the same mission on board (a battery
    /// swap); a mission changed in between, from UXDuck or anything else,
    /// starts a new run (maintainer, 2026-10-24).
    fn flight_mission(&self, flight: &Flight, complete: bool) -> FlightMission {
        use uxduck_core::domain::mission::MissionOrigin;
        let record = self.mission_history(&flight.vehicle_id).into_iter().next();
        let (plan_id, plan_name) = match record.map(|r| &r.origin) {
            Some(MissionOrigin::Uploaded {
                plan_id, plan_name, ..
            }) => (plan_id.clone(), plan_name.clone().filter(|n| !n.is_empty())),
            _ => (None, None),
        };
        let mission_id = record.map(|r| r.id.clone());
        let previous = self
            .state
            .flights
            .values()
            .filter(|f| f.vehicle_id == flight.vehicle_id && f.id != flight.id)
            .filter(|f| f.armed_at_us < flight.armed_at_us)
            .max_by_key(|f| f.armed_at_us);
        let run_id = previous
            .and_then(|f| f.mission.as_ref())
            .filter(|m| !m.completed && m.mission_id.is_some() && m.mission_id == mission_id)
            .map_or_else(|| flight.id.clone(), |m| m.run_id.clone());
        FlightMission {
            mission_id,
            plan_id,
            plan_name,
            completed: complete,
            run_id,
        }
    }

    /// The unsaved flights of a mission run, oldest first.
    fn run_flights(&self, run_id: &str) -> Vec<&Flight> {
        let mut run: Vec<&Flight> = self
            .state
            .flights
            .values()
            .filter(|f| f.mission.as_ref().is_some_and(|m| m.run_id == run_id))
            .collect();
        run.sort_by_key(|f| f.armed_at_us);
        run
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

    /// A vehicle went offline or was heard again (C8): its open flight
    /// notes when it was last heard, and carries on when it is heard again.
    /// It never ends by itself, since a link can drop and come back
    /// (maintainer, 2026-10-24); someone can end it (`flight/end`).
    pub(crate) fn flight_presence(&mut self, vehicle_id: &str) {
        let Some(vehicle) = self.vehicle(vehicle_id) else {
            return;
        };
        let (id, presence, heard, armed) = (
            vehicle.id.clone(),
            vehicle.presence,
            vehicle.last_heartbeat_us,
            vehicle.armed,
        );
        let Some(flight_id) = self.flight_times.0.get(&id).and_then(|t| t.flight.clone()) else {
            // Its flight was ended while it was away: heard armed, a new one begins.
            if armed && presence == uxduck_core::domain::vehicle::Presence::Online {
                self.review_flight_time(&id);
            }
            return;
        };
        let Some(mut flight) = self.state.flights.get(&flight_id).cloned() else {
            return;
        };
        let lost = match presence {
            uxduck_core::domain::vehicle::Presence::Offline => Some(heard.max(flight.armed_at_us)),
            uxduck_core::domain::vehicle::Presence::Online => None,
            uxduck_core::domain::vehicle::Presence::Stale => return,
        };
        if flight.contact_lost_at_us == lost {
            return;
        }
        flight.contact_lost_at_us = lost;
        flight.version += 1;
        self.commit_as(flight_actor(), Change::FlightUpserted { flight });
    }

    /// A vehicle's open flight, if it has one.
    pub(crate) fn open_flight(&self, vehicle_id: &str) -> Option<&Flight> {
        let flight_id = self.flight_times.0.get(vehicle_id)?.flight.as_ref()?;
        self.state.flights.get(flight_id)
    }

    /// `flight/end`: someone ends an open flight whose vehicle cannot be
    /// heard, at when it was last heard. Heard again still armed, it begins
    /// a new flight.
    pub(crate) fn end_lost_flight(
        &mut self,
        id: &str,
    ) -> Result<uxduck_contracts::dto::flight::Flight, uxduck_contracts::errors::ApiError> {
        use uxduck_contracts::errors::{ApiError, ErrorKind};
        let flight = self
            .state
            .flights
            .get(id)
            .cloned()
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no flight {id}")))?;
        if flight.end != FlightEnd::Open {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "That flight has already ended.",
            ));
        }
        let Some(lost_at) = flight.contact_lost_at_us else {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "UXDuck can still hear this vehicle. Disarm it to end the flight.",
            ));
        };
        self.end_flight(&flight.vehicle_id, FlightEnd::ContactLost, lost_at);
        if let Some(timer) = self.flight_times.0.get_mut(&flight.vehicle_id) {
            *timer = Timer::default();
        }
        self.drop_end_button(id);
        let ended = self.state.flights.get(id).ok_or_else(|| {
            ApiError::new(ErrorKind::Internal, "the flight went missing as it ended")
        })?;
        Ok(ended.into())
    }

    /// The offline alarm's End flight button goes once the flight has ended.
    fn drop_end_button(&mut self, flight_id: &str) {
        use uxduck_core::domain::notification::Action;
        let stale: Vec<_> = self
            .state
            .notifications
            .values()
            .filter(|n| {
                n.actions.iter().any(
                    |a| matches!(&a.action, Action::EndFlight { flight_id: f } if f == flight_id),
                )
            })
            .cloned()
            .collect();
        for mut notification in stale {
            notification
                .actions
                .retain(|a| !matches!(&a.action, Action::EndFlight { .. }));
            notification.version += 1;
            self.commit_as(
                crate::plugins::daemon(),
                Change::NotificationUpserted { notification },
            );
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
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "small enums"
            )]
            let (mission_state, mission_mode) = (
                self.telemetry_value(&id, "mission.state").map(|v| v as u8),
                self.telemetry_value(&id, "mission.mode").map(|v| v as u8),
            );
            if let Some(timer) = self.flight_times.0.get_mut(&id)
                && timer.flight.is_some()
            {
                timer.max_height_m = higher(timer.max_height_m, height);
                timer.max_speed_mps = higher(timer.max_speed_mps, speed);
                // MISSION_CURRENT: state 3 active, 5 complete; mode 1 in a mission mode.
                match (mission_state, mission_mode) {
                    (Some(3), _) | (_, Some(1)) => timer.mission_flown = true,
                    _ => {}
                }
                if mission_state == Some(5) && timer.mission_flown {
                    timer.mission_complete = true;
                }
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

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
//!   A hop ends once the vehicle has stayed down for [`LANDED_AFTER_US`],
//!   timed from when it came down: a land detector that says "on the
//!   ground" for a moment in flight, as some simulators' do, neither ends
//!   the hop nor costs it time ([`Hop`]).
//!
//! Sent with the vehicle's telemetry, at most once a second each, never
//! journaled; a daemon start forgets them.
//!
//! Each flight also adds up its time moving (FB-228), for every vehicle:
//! while armed with its ground speed above [`MOVING_ABOVE_MPS`]. Kept with
//! the flight when it ends, not sent as telemetry.
//!
//! Each arm to disarm is also kept as a [`Flight`] (C8), journaled when it
//! begins and when it ends, with its highest point, top speed, modes,
//! battery and who acted on the vehicle; one left open by a crash is ended
//! at the next start, where it was last heard.

use std::collections::BTreeMap;

use uxduck_core::domain::command::FLYING_ABOVE_HOME_M;
use uxduck_core::domain::modes::Family;
use uxduck_core::domain::telemetry::{TelemetryItem, Unit, VehicleTelemetry, off_the_ground};
use uxduck_core::domain::telemetry_layout::VehicleClass;

use uxduck_contracts::dto::analytics::DaemonEvent;
use uxduck_contracts::dto::flight::place_of;
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::flight::{Flight, FlightEnd, FlightMission, FlightOperator};
use uxduck_core::domain::settings::SyncContent;
use uxduck_core::events::{Actor, Change};

use crate::Engine;

/// Off the ground above this height above home, with no word from the autopilot.
const AIR_ABOVE_M: f64 = FLYING_ABOVE_HOME_M;
/// Back on the ground below this, so a hover at the line does not flicker.
const GROUND_BELOW_M: f64 = 1.0;
/// Moving above this ground speed (FB-228): above GPS drift standing
/// still, and the same as a phone's NMEA position counts moving.
const MOVING_ABOVE_MPS: f64 = 0.5;
/// Down for this long is a landing. A shorter spell is the same hop still.
const LANDED_AFTER_US: u64 = 3_000_000;

const SOURCE: &str = "flight";

/// Why a simulated flight is not synced on a free plan (ADR-0046 B6).
const SIMULATED_ON_PAID: &str = "Simulated flights sync on a paid plan.";
/// Why a flight's video is not synced on a free plan (B4).
const VIDEO_ON_PAID: &str = "Video syncs on a paid plan.";

/// The longest name a flight, or a replay, takes.
pub(crate) const MAX_FLIGHT_NAME_CHARS: usize = 80;

/// Each vehicle's flight time, by vehicle id.
#[derive(Debug, Default)]
pub(crate) struct FlightTimes(BTreeMap<String, Timer>);

/// One stretch off the ground, as the flight timer and a replay's marks
/// follow it. It begins the moment the vehicle is off the ground, and ends
/// only once the vehicle has stayed down for [`LANDED_AFTER_US`], from when
/// it came down. Down for less than that and up again, it is one hop that
/// counts the time between.
#[derive(Debug, Default)]
pub(crate) struct Hop {
    /// Off the ground since.
    since_us: Option<u64>,
    /// Down again since, and not yet for long enough to have landed.
    down_since_us: Option<u64>,
}

/// What a look at the vehicle did to its hop.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HopChange {
    /// It left the ground.
    Lifted,
    /// It landed: off the ground from and until these.
    Landed { from_us: u64, until_us: u64 },
}

impl Hop {
    /// In the air: off the ground, or down too briefly to have landed.
    pub(crate) fn flying(&self) -> bool {
        self.since_us.is_some()
    }

    /// Off the ground as last looked at, for the band between the two
    /// heights, which keeps what was.
    pub(crate) fn up(&self) -> bool {
        self.since_us.is_some() && self.down_since_us.is_none()
    }

    /// How long the hop has lasted, stopped while the vehicle is down.
    fn lasted_us(&self, now_us: u64) -> u64 {
        self.since_us.map_or(0, |from| {
            self.down_since_us
                .map_or(now_us, |down| down.min(now_us))
                .saturating_sub(from)
        })
    }

    /// The vehicle is `up`, off the ground, or not, at `now_us`.
    pub(crate) fn observe(&mut self, up: bool, now_us: u64) -> Option<HopChange> {
        let Some(from_us) = self.since_us else {
            if !up {
                return None;
            }
            self.since_us = Some(now_us);
            return Some(HopChange::Lifted);
        };
        if up {
            self.down_since_us = None;
            return None;
        }
        let until_us = *self.down_since_us.get_or_insert(now_us);
        if now_us.saturating_sub(until_us) < LANDED_AFTER_US {
            return None;
        }
        *self = Self::default();
        Some(HopChange::Landed { from_us, until_us })
    }

    /// The flight or the log ends at `now_us`, and the hop with it: from
    /// and until when it was off the ground, if it was.
    pub(crate) fn end(&mut self, now_us: u64) -> Option<(u64, u64)> {
        let from_us = self.since_us.take()?;
        let until_us = self
            .down_since_us
            .take()
            .map_or(now_us, |down| down.min(now_us));
        Some((from_us, until_us))
    }
}

#[derive(Debug, Default)]
struct Timer {
    /// While armed: when.
    armed_at_us: Option<u64>,
    /// After a disarm: how long it was armed.
    armed_us: u64,
    /// Its hop off the ground now.
    hop: Hop,
    /// Time in the air banked before the current hop.
    air_us: u64,
    /// While moving: since when.
    moving_since_us: Option<u64>,
    /// Time moving banked before.
    moving_us: u64,
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
    /// `mission.reached` (its item and time) as the flight first saw it,
    /// once it has looked, and the item it reached since (S343).
    reached_looked: bool,
    reached_before: Option<(f64, u64)>,
    reached: Option<f64>,
}

impl Timer {
    fn armed_us(&self, now_us: u64) -> u64 {
        self.armed_at_us
            .map_or(self.armed_us, |at| now_us.saturating_sub(at))
    }

    fn air_us(&self, now_us: u64) -> u64 {
        self.air_us + self.hop.lasted_us(now_us)
    }

    fn moving_us(&self, now_us: u64) -> u64 {
        self.moving_us
            + self
                .moving_since_us
                .map_or(0, |at| now_us.saturating_sub(at))
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

/// How long a flight was armed: all of it, in the air or not.
/// A `flight/list` page's `older`: when the page's oldest flight armed and
/// its id, which the next page starts after. It stays good when that flight
/// is deleted.
fn page_cursor(flight: &Flight) -> String {
    format!("{}:{}", flight.armed_at_us, flight.id)
}

fn parse_page_cursor(text: &str) -> Option<(u64, String)> {
    let (us, id) = text.split_once(':')?;
    if us.is_empty() || !us.bytes().all(|b| b.is_ascii_digit()) || id.is_empty() {
        return None;
    }
    Some((us.parse().ok()?, id.to_owned()))
}

fn armed_for_us(flight: &Flight) -> u64 {
    flight
        .ended_at_us
        .unwrap_or(flight.armed_at_us)
        .saturating_sub(flight.armed_at_us)
}

/// A flight in words for the vehicle's history: "Flew 12 min, highest
/// 45 m", or "Armed 3 min" for one that never left the ground or water.
fn flight_words(flight: &Flight) -> String {
    let span = |us: u64| match us / 60_000_000 {
        0 => format!("{} s", us / 1_000_000),
        m => format!("{m} min"),
    };
    let armed = armed_for_us(flight);
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

/// What a synced flight asks for besides its summary (ADR-0046 B1, B11):
/// its log, at logs or video, when it was recorded here and has ended; its
/// video, at video. Never takes back what it asked before. Whether that
/// changed it.
fn ask_content(flight: &mut Flight, content: SyncContent) -> bool {
    use uxduck_core::domain::flight::FlightLog;
    let log = content >= SyncContent::Logs
        && flight.recorded_here
        && flight.link_id.is_some()
        && flight.end != FlightEnd::Open
        && flight.log == FlightLog::None;
    if log {
        flight.log = FlightLog::Asked;
    }
    let video = content == SyncContent::Video && flight.recorded_here && !flight.sync_video;
    if video {
        flight.sync_video = true;
    }
    log || video
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

    pub(crate) fn telemetry_value(&self, vehicle_id: &str, key: &str) -> Option<f64> {
        self.telemetry(vehicle_id)
            .and_then(|t| t.items.iter().find(|i| i.key == key))
            .and_then(|i| i.value)
    }

    /// A flight begins (C8), journaled now so a crash cannot lose it.
    fn begin_flight(&mut self, vehicle_id: &str, armed_at_us: u64, seen_armed: bool) {
        let Some(vehicle) = self.vehicle(vehicle_id).cloned() else {
            return;
        };
        // By its link, its own word or the person's (ADR-0039 R14).
        let simulated = self.is_simulated(&vehicle);
        let mut flight = Flight {
            copies: Vec::new(),
            id: format!("fl{}", &self.random.token()[..16]),
            vehicle_id: vehicle.id.clone(),
            system_id: Some(vehicle.system_id),
            vehicle_name: vehicle.name.clone(),
            vehicle_type: vehicle.vehicle_type.clone(),
            name: None,
            simulated,
            seen_armed,
            armed_at_us,
            ended_at_us: None,
            end: FlightEnd::Open,
            air_us: 0,
            moving_us: Some(0),
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
            link_id: Some(vehicle.link_id.clone()),
            // Where its connection's log is now, for its replay (ADR-0039 R10).
            tlog: self
                .system
                .tlog_position(&vehicle.link_id)
                .map(|(file, offset)| uxduck_core::domain::flight::FlightTlog { file, offset }),
            tlog_end: None,
            other_links: Vec::new(),
            log: uxduck_core::domain::flight::FlightLog::None,
            sync_video: false,
            mission: None,
            params: crate::param_definitions::queries::armed_with(self, &vehicle.id),
            checklist: None,
            version: 1,
        };
        self.add_heard_links(&mut flight, &vehicle);
        if let Some(timer) = self.flight_times.0.get_mut(&vehicle.id) {
            timer.flight = Some(flight.id.clone());
            timer.modes.clone_from(&flight.modes);
        }
        self.commit_as(flight_actor(), Change::FlightUpserted { flight });
    }

    /// Adds to a flight the connections its vehicle is heard on in this run
    /// besides its own (ADR-0039 R10, 2026-11-22), each with where its log
    /// is now. Whether it added any.
    fn add_heard_links(
        &self,
        flight: &mut Flight,
        vehicle: &uxduck_core::domain::vehicle::Vehicle,
    ) -> bool {
        let Some(connections) = &vehicle.connections else {
            return false;
        };
        let mut added = false;
        for route in connections.routes.iter().filter(|r| !r.remembered) {
            let link = route.link_id.as_str();
            if flight.link_id.as_deref() == Some(link)
                || flight.other_links.iter().any(|l| l.link_id == link)
            {
                continue;
            }
            flight
                .other_links
                .push(uxduck_core::domain::flight::FlightLink {
                    link_id: link.to_owned(),
                    tlog: self.system.tlog_position(link).map(|(file, offset)| {
                        uxduck_core::domain::flight::FlightTlog { file, offset }
                    }),
                });
            added = true;
        }
        added
    }

    /// A vehicle was heard: its open flight takes any connection it is
    /// heard on now that the flight has not yet (ADR-0039 R10, 2026-11-22).
    pub(crate) fn flight_heard_on(&mut self, vehicle: &uxduck_core::domain::vehicle::Vehicle) {
        let Some(flight_id) = self
            .flight_times
            .0
            .get(&vehicle.id)
            .and_then(|t| t.flight.clone())
        else {
            return;
        };
        let Some(mut flight) = self.state.flights.get(&flight_id).cloned() else {
            return;
        };
        if self.add_heard_links(&mut flight, vehicle) {
            flight.version += 1;
            self.commit_as(flight_actor(), Change::FlightUpserted { flight });
        }
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
        let moving_us = timer.moving_us(at_us);
        let (height, speed, modes) = (
            timer.max_height_m,
            timer.max_speed_mps,
            std::mem::take(&mut timer.modes),
        );
        let (flown, complete) = (
            std::mem::take(&mut timer.mission_flown),
            std::mem::take(&mut timer.mission_complete),
        );
        timer.reached_looked = false;
        timer.reached_before = None;
        let reached = timer.reached.take();
        let Some(mut flight) = self.state.flights.get(&flight_id).cloned() else {
            return;
        };
        flight.ended_at_us = Some(at_us);
        flight.end = end;
        // Where its log ends, so its size is its own (FB-148). One ended
        // after contact was lost ends when it was last heard, and where the
        // link wrote then is not kept, so it has no mark.
        flight.tlog_end = flight
            .link_id
            .as_deref()
            .filter(|_| end != FlightEnd::ContactLost)
            .and_then(|link| self.system.tlog_position(link))
            .map(|(file, offset)| uxduck_core::domain::flight::FlightTlog { file, offset });
        flight.contact_lost_at_us = None;
        flight.air_us = air_us;
        flight.moving_us = Some(moving_us);
        flight.max_height_m = height;
        flight.max_speed_mps = speed;
        flight.modes = modes;
        flight.battery_end_pct = self.telemetry_value(vehicle_id, "battery.remaining_pct");
        flight.operators = self.flight_operators(vehicle_id, flight.armed_at_us);
        flight.mission = flown.then(|| self.flight_mission(&flight, complete));
        // "Sync real flights automatically" (ADR-0046 B8): a real flight is
        // kept as it ends, with what "What to sync" says (B11). "Sync
        // simulated flights" (B17) does the same for a simulated one, on a
        // plan that takes them (B6). Otherwise it waits for its row's cloud,
        // and nothing asks (B16).
        let settings = &self.state.settings;
        let automatic = !flight.keep
            && if flight.simulated {
                settings.sync_simulated && self.may_sync_simulated()
            } else {
                settings.save_flights || settings.store_flights
            };
        if automatic {
            flight.keep = true;
            ask_content(&mut flight, self.sync_content(None));
        }
        flight.version += 1;
        self.commit_as(
            flight_actor(),
            Change::FlightUpserted {
                flight: flight.clone(),
            },
        );
        if automatic {
            self.track(DaemonEvent::FlightSynced {
                simulated: flight.simulated,
                automatic: true,
            });
        }
        let mut text = flight_words(&flight);
        if flown {
            text.push_str(&self.mission_words(vehicle_id, complete, reached));
        }
        self.record(
            vehicle_id,
            uxduck_core::domain::history::HistoryEvent::FlightEnded {
                flight_id: flight.id.clone(),
                text,
            },
        );
        // Two vehicles flying together are one operation (ADR-0039 R16).
        self.regroup_operations();
    }

    /// `flight/list`: the newest flights a page at a time, or the ones
    /// asked for by id, and totals over all of them (C9).
    ///
    /// # Errors
    /// When `before` is not an `older` this method gave.
    pub(crate) fn list_flights(
        &self,
        params: &uxduck_contracts::dto::flight::FlightListParams,
    ) -> Result<uxduck_contracts::dto::flight::FlightListResult, ApiError> {
        let limit = usize::try_from(params.limit.unwrap_or(100).clamp(1, 500)).unwrap_or(100);
        let before = params
            .before
            .as_deref()
            .map(|text| {
                parse_page_cursor(text).ok_or_else(|| {
                    ApiError::new(
                        ErrorKind::InvalidParams,
                        "`before` must be an `older` from an earlier flight/list.",
                    )
                })
            })
            .transpose()?;
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
        let operators = self.logbook_operators(&flights);
        let places = self.logbook_places(&flights);
        if let Some(id) = &params.operator_id {
            flights.retain(|f| f.operators.iter().any(|o| &o.operator_id == id));
        }
        if params.here {
            let moved = self.moved_videos();
            flights
                .retain(|f| f.recorded_here && !moved.iter().any(|r| self.is_flight_video(f, r)));
        }
        if let Some(place) = &params.place {
            flights.retain(|f| f.copies.iter().any(|c| &place_of(&c.path) == place));
        }
        if let Some(folder) = &params.under {
            flights.retain(|f| {
                f.copies
                    .iter()
                    .any(|c| crate::offload::inside(folder, &c.path))
            });
        }
        // Real or simulated (FB-148), and when and how long (FB-226), over
        // every entry, so each page and the totals hold only the matches.
        flights.retain(|f| {
            params
                .simulated
                .is_none_or(|simulated| f.simulated == simulated)
                && params.armed_since_us.is_none_or(|us| f.armed_at_us >= us)
                && params.armed_before_us.is_none_or(|us| f.armed_at_us < us)
        });
        if params.longer_than_us.is_some() || params.shorter_than_us.is_some() {
            let now = self.clock.now_us();
            flights.retain(|f| {
                let armed = if f.end == FlightEnd::Open {
                    now.saturating_sub(f.armed_at_us)
                } else {
                    armed_for_us(f)
                };
                params.longer_than_us.is_none_or(|us| armed > us)
                    && params.shorter_than_us.is_none_or(|us| armed < us)
            });
        }
        // Newest first, and the same order every time, so a page ends where
        // the next begins even when two flights armed together.
        flights.sort_by(|a, b| (b.armed_at_us, &b.id).cmp(&(a.armed_at_us, &a.id)));
        let totals = logbook_totals(&flights, params.totals_since_us);
        let (page, older) = if params.ids.is_empty() {
            let mut page: Vec<_> = flights
                .into_iter()
                .filter(|f| {
                    before
                        .as_ref()
                        .is_none_or(|(us, id)| (f.armed_at_us, f.id.as_str()) < (*us, id.as_str()))
                })
                .take(limit + 1)
                .collect();
            let older = (page.len() > limit).then(|| page_cursor(page[limit - 1]));
            page.truncate(limit);
            (page, older)
        } else {
            let ids: std::collections::HashSet<&str> =
                params.ids.iter().map(String::as_str).collect();
            let found = flights
                .into_iter()
                .filter(|f| ids.contains(f.id.as_str()))
                .collect();
            (found, None)
        };
        // What each takes here (FB-148), each link's files read once.
        let mut logs = BTreeMap::new();
        let copied = self.system.copied_logs();
        Ok(uxduck_contracts::dto::flight::FlightListResult {
            flights: page
                .into_iter()
                .map(|flight| self.listed_flight(flight, &mut logs, &copied))
                .collect(),
            older,
            totals,
            operators,
            places,
        })
    }

    /// The disks and folders holding the entries' copies, by name, each
    /// with how many and whether any copy there is there now (S379).
    fn logbook_places(
        &self,
        flights: &[&Flight],
    ) -> Vec<uxduck_contracts::dto::flight::FlightListPlace> {
        let mut places: BTreeMap<String, (u32, bool)> = BTreeMap::new();
        for flight in flights {
            let mut seen = std::collections::BTreeSet::new();
            for copy in &flight.copies {
                let place = place_of(&copy.path);
                let entry = places.entry(place.clone()).or_insert((0, true));
                if self.system.file_present(&copy.path) != Some(false) {
                    entry.1 = false;
                }
                if seen.insert(place) {
                    entry.0 += 1;
                }
            }
        }
        places
            .into_iter()
            .map(
                |(place, (count, unplugged))| uxduck_contracts::dto::flight::FlightListPlace {
                    place,
                    count,
                    unplugged,
                },
            )
            .collect()
    }

    /// The recordings moved to a disk (ADR-0062): no longer on this computer.
    fn moved_videos(&self) -> Vec<&uxduck_core::domain::video::VideoRecording> {
        self.state
            .video_recordings
            .values()
            .filter(|r| r.packed.is_some())
            .collect()
    }

    /// Who acted on the entries, over every entry whatever the operator
    /// filter, each named as now, else as on their newest entry (FB-228).
    fn logbook_operators(
        &self,
        flights: &[&Flight],
    ) -> Vec<uxduck_contracts::dto::flight::FlightListOperator> {
        let mut named: BTreeMap<&str, (u64, &str, u32)> = BTreeMap::new();
        for flight in flights {
            let mut seen = std::collections::BTreeSet::new();
            for operator in &flight.operators {
                if !seen.insert(operator.operator_id.as_str()) {
                    continue;
                }
                let entry = named.entry(&operator.operator_id).or_insert((
                    flight.armed_at_us,
                    &operator.name,
                    0,
                ));
                entry.2 += 1;
                if flight.armed_at_us > entry.0 {
                    (entry.0, entry.1) = (flight.armed_at_us, &operator.name);
                }
            }
        }
        let mut operators: Vec<_> = named
            .into_iter()
            .map(
                |(id, (_, name, count))| uxduck_contracts::dto::flight::FlightListOperator {
                    operator_id: id.to_owned(),
                    name: self
                        .state
                        .operators
                        .get(id)
                        .map_or(name, |o| o.name.as_str())
                        .to_owned(),
                    count,
                },
            )
            .collect();
        operators.sort_by(|a, b| (&a.name, &a.operator_id).cmp(&(&b.name, &b.operator_id)));
        operators
    }

    /// An entry as `flight/list` gives it: what it takes here (FB-148) and
    /// the autopilot logs copied off cards that cover it (S426).
    fn listed_flight(
        &self,
        flight: &Flight,
        logs: &mut BTreeMap<String, Vec<crate::runtime::LinkLog>>,
        copied: &[uxduck_contracts::dto::replay::CopiedLog],
    ) -> uxduck_contracts::dto::flight::Flight {
        uxduck_contracts::dto::flight::Flight {
            size: Some(self.flight_size(flight, logs)),
            autopilot_logs: copied
                .iter()
                .filter(|log| self.log_covers(log, flight))
                .map(|log| uxduck_contracts::dto::flight::FlightAutopilotLog {
                    path: log.path.clone(),
                    name: log.name.clone(),
                })
                .collect(),
            ..self.flight_dto(flight)
        }
    }

    /// Whether an autopilot log copied off a card covers a flight (S426):
    /// the system id it names is its vehicle's, and some of the flight's
    /// armed time is inside the log's span. Only a log on GPS time can be
    /// placed, and only a flight of a vehicle known here has a system id.
    fn log_covers(&self, log: &uxduck_contracts::dto::replay::CopiedLog, flight: &Flight) -> bool {
        let Some(span) = log.span.as_ref().filter(|span| span.gps_time) else {
            return false;
        };
        let Some(vehicle) = self.vehicle(&flight.vehicle_id) else {
            return false;
        };
        let ended = flight.ended_at_us.unwrap_or_else(|| self.clock.now_us());
        vehicle.system_id == span.system_id
            && span.from_us <= ended
            && flight.armed_at_us <= span.to_us
    }

    /// `logs/cards`: each copied log with the Logbook's flights it covers,
    /// newest first (S426).
    pub(crate) fn match_copied_logs(&self, logs: &mut [uxduck_contracts::dto::replay::CopiedLog]) {
        let mut flights: Vec<&Flight> = self.state.flights.values().collect();
        flights.sort_by(|a, b| (b.armed_at_us, &b.id).cmp(&(a.armed_at_us, &a.id)));
        for log in logs {
            log.flights = flights
                .iter()
                .filter(|flight| self.log_covers(log, flight))
                .map(|flight| uxduck_contracts::dto::replay::LogFlight {
                    id: flight.id.clone(),
                    name: flight
                        .name
                        .clone()
                        .unwrap_or_else(|| flight.vehicle_name.clone()),
                    armed_at_us: flight.armed_at_us,
                })
                .collect();
        }
    }

    fn flight_dto(&self, flight: &Flight) -> uxduck_contracts::dto::flight::Flight {
        let mut dto: uxduck_contracts::dto::flight::Flight = flight.into();
        dto.system_id = dto
            .system_id
            .or_else(|| self.vehicle(&flight.vehicle_id).map(|v| v.system_id));
        dto.replay_available = self
            .flight_replay_read(flight, None)
            .and_then(|read| self.system.replay_available(&read));
        self.offload_dto(&mut dto);
        dto
    }

    pub(crate) fn remove_flight(
        &mut self,
        connection: crate::ConnectionId,
        id: &str,
        video: bool,
    ) -> Result<(), ApiError> {
        let flight = self
            .state
            .flights
            .get(id)
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no flight {id}")))?;
        if flight.end == FlightEnd::Open {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                "End the flight before removing it from the logbook.",
            ));
        }
        let keep = flight.keep;
        let only = std::iter::once(id).collect();
        let (freed, _) = self.freed_link_logs(&only);
        let recordings = if video {
            self.freed_recordings(&only)
        } else {
            Vec::new()
        };
        for recording in &recordings {
            let deleted = match self.video.port.as_mut() {
                Some(port) => port.delete_recording(&recording.id, &recording.file()),
                None => Err("Recordings need UXDuck Desktop.".into()),
            };
            deleted.map_err(|why| {
                ApiError::new(
                    ErrorKind::Internal,
                    format!("Could not delete its video. {why} The entry is still here."),
                )
            })?;
            self.forget(recording.clone());
        }
        for (link, log) in freed {
            self.system
                .remove_link_log(&link, &log.name)
                .map_err(|why| {
                    ApiError::new(
                        ErrorKind::Internal,
                        format!("Could not delete its telemetry recording. {why} The entry is still here."),
                    )
                })?;
        }
        if keep {
            self.commit(connection, Change::FlightRemoved { id: id.to_owned() });
        }
        self.commit(connection, Change::FlightForgotten { id: id.to_owned() });
        self.regroup_operations();
        Ok(())
    }

    /// `flight/removal`: what deleting these flights would free here (S348).
    pub(crate) fn flight_removal(
        &self,
        ids: &[String],
    ) -> Result<
        uxduck_contracts::dto::flight::FlightRemovalResult,
        uxduck_contracts::errors::ApiError,
    > {
        use uxduck_contracts::errors::{ApiError, ErrorKind};
        if let Some(missing) = ids.iter().find(|id| !self.state.flights.contains_key(*id)) {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                format!("no flight {missing}"),
            ));
        }
        let ids = ids.iter().map(String::as_str).collect();
        let (freed, kept_bytes) = self.freed_link_logs(&ids);
        Ok(uxduck_contracts::dto::flight::FlightRemovalResult {
            freed_bytes: freed.iter().map(|(_, log)| log.bytes).sum(),
            kept_bytes,
            video_bytes: self.freed_recordings(&ids).iter().map(|r| r.bytes).sum(),
        })
    }

    /// `flight/rename`: a person's name for a flight, or with an empty one
    /// its vehicle's again. A kept flight's name goes to the account too.
    pub(crate) fn rename_flight(
        &mut self,
        connection: crate::ConnectionId,
        id: &str,
        name: &str,
    ) -> Result<uxduck_contracts::dto::flight::Flight, uxduck_contracts::errors::ApiError> {
        use uxduck_contracts::errors::{ApiError, ErrorKind};
        let name = Some(name.trim())
            .filter(|n| !n.is_empty())
            .map(str::to_owned);
        if name
            .as_ref()
            .is_some_and(|n| n.chars().count() > MAX_FLIGHT_NAME_CHARS)
        {
            return Err(ApiError::new(
                ErrorKind::InvalidValue,
                format!("a flight's name has up to {MAX_FLIGHT_NAME_CHARS} characters"),
            ));
        }
        let mut flight = self
            .state
            .flights
            .get(id)
            .cloned()
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no flight {id}")))?;
        if flight.name != name {
            flight.name = name;
            flight.version += 1;
            self.commit(
                connection,
                Change::FlightUpserted {
                    flight: flight.clone(),
                },
            );
            self.replay_flight_renamed(id);
        }
        Ok(self.flight_dto(&flight))
    }

    /// Why a flight's video does not sync now: the free plan's video
    /// allowance used, with what the account keeps (ADR-0069 P2, P11), or
    /// a plan that takes no video (B4).
    fn video_refused(&self) -> String {
        let status = self.upload_status.as_ref();
        let mb = |bytes: u64| bytes.div_ceil(1_000_000);
        match status.and_then(|s| s.video_quota_bytes.zip(s.video_bytes)) {
            Some((quota, used)) => format!(
                "Your free {} MB of video is used: {} MB in your account.",
                mb(quota),
                mb(used)
            ),
            None => VIDEO_ON_PAID.into(),
        }
    }

    /// What a flight synced now takes (ADR-0046 B11): "What to sync", or
    /// more when asked for this once, and no video on a plan without it.
    fn sync_content(&self, asked: Option<SyncContent>) -> SyncContent {
        let content = asked.map_or(self.state.settings.sync_content, |asked| {
            asked.max(self.state.settings.sync_content)
        });
        if self.may_upload_video() {
            content
        } else {
            content.min(SyncContent::Logs)
        }
    }

    /// Syncs a flight, its summary with what "What to sync" says or
    /// `content` asks this once (ADR-0046 B1, B11), adding what a synced one
    /// lacks; or deletes it from the account (B2). The flight stays here
    /// either way.
    pub(crate) fn keep_flight(
        &mut self,
        connection: crate::ConnectionId,
        id: &str,
        keep: bool,
        always: bool,
        content: Option<SyncContent>,
    ) -> Result<uxduck_contracts::dto::flight::Flight, uxduck_contracts::errors::ApiError> {
        use uxduck_contracts::errors::{ApiError, ErrorKind};
        let mut flight = self
            .state
            .flights
            .get(id)
            .cloned()
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no flight {id}")))?;
        if keep && !flight.keep && flight.simulated && !self.may_sync_simulated() {
            return Err(ApiError::new(ErrorKind::InvalidValue, SIMULATED_ON_PAID));
        }
        if keep && content == Some(SyncContent::Video) && !self.may_upload_video() {
            return Err(ApiError::new(ErrorKind::InvalidValue, self.video_refused()));
        }
        let wanted = self.sync_content(content);
        let synced = DaemonEvent::FlightSynced {
            simulated: flight.simulated,
            automatic: false,
        };
        if keep && flight.left_account {
            // Saved again after a delete (2026-10-24): never by itself, only
            // when asked, and under a new id the account has no removal for.
            // Its log goes again under that id (B1).
            let mut again = Flight {
                id: format!("fl{}", &self.random.token()[..16]),
                keep: true,
                left_account: false,
                log: uxduck_core::domain::flight::FlightLog::None,
                sync_video: false,
                version: 1,
                ..flight
            };
            ask_content(&mut again, wanted);
            self.commit(
                connection,
                Change::FlightReplaced {
                    old_id: id.to_owned(),
                    flight: again.clone(),
                },
            );
            self.track(synced);
            return Ok(self.flight_dto(&again));
        }
        // Synced now, or synced before and now with more: its log, or its
        // video (B11).
        let newly = keep && !flight.keep;
        let more = keep && ask_content(&mut flight, wanted);
        if newly || more {
            flight.keep = true;
            flight.version += 1;
            self.commit(
                connection,
                Change::FlightUpserted {
                    flight: flight.clone(),
                },
            );
        }
        if newly {
            self.track(synced.clone());
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
                ask_content(&mut other, wanted);
                other.version += 1;
                self.commit(connection, Change::FlightUpserted { flight: other });
                self.track(synced.clone());
            }
        } else if !keep && flight.keep {
            // Deleted from the account (2026-10-24): there is no "unsave".
            self.commit(connection, Change::FlightRemoved { id: id.to_owned() });
            // Its log and video are no longer asked for, and what waits of
            // its log goes (B2).
            if let Some(mut here) = self.state.flights.get(id).cloned()
                && (here.log != uxduck_core::domain::flight::FlightLog::None || here.sync_video)
            {
                here.log = uxduck_core::domain::flight::FlightLog::None;
                here.sync_video = false;
                here.version += 1;
                self.commit(connection, Change::FlightUpserted { flight: here });
            }
        }
        if always
            && (self.state.settings.save_flights != keep
                || self.state.settings.store_flights != keep)
        {
            // One setting (B8), which answers the Logbook's question (B16).
            let mut settings = self.state.settings.clone();
            settings.sync_automatically(keep);
            settings.version += 1;
            self.commit(connection, Change::SettingsChanged { settings });
        }
        // Its operation goes to the account with it (R16).
        self.regroup_operations();
        // Gone from here too when it only came from the account.
        let after = self.state.flights.get(id).cloned().unwrap_or(Flight {
            keep: false,
            ..flight
        });
        Ok(self.flight_dto(&after))
    }

    /// The mission a flight just flew: the one UXDuck last knew on the
    /// vehicle, by its content (S134), and the plan it came from when UXDuck
    /// uploaded it. The run goes on from the vehicle's last flight only when
    /// that one landed mid-mission with the same mission on board (a battery
    /// swap); a mission changed in between, from UXDuck or anything else,
    /// starts a new run (maintainer, 2026-10-24).
    /// How far through its mission a flight got, for its History entry
    /// (S343): ", mission complete", or ", reached mission step 4 of 6",
    /// counted as the Cockpit counts steps; nothing when it reached none.
    fn mission_words(&self, vehicle_id: &str, complete: bool, reached: Option<f64>) -> String {
        if complete {
            return ", mission complete".into();
        }
        let Some(item) = reached else {
            return String::new();
        };
        let px4 = self.vehicle(vehicle_id).is_some_and(|v| {
            Family::for_vehicle(&v.autopilot, &v.vehicle_type) == Some(Family::Px4)
        });
        #[allow(clippy::cast_possible_truncation, reason = "a mission item number")]
        let step = item.round() as i64 + i64::from(px4);
        let held = self.mission(vehicle_id).items.len();
        let steps = if px4 { held } else { held.saturating_sub(1) };
        if step < 1 {
            return String::new();
        }
        if steps == 0 {
            return format!(", reached mission step {step}");
        }
        format!(", reached mission step {step} of {steps}")
    }

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

    /// Flights a crash left open, ended where their vehicle was last heard:
    /// by its last heartbeat here, or its link's log, whichever is later.
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
            let logged = flight
                .link_id
                .as_deref()
                .and_then(|link| self.system.link_last_heard_us(link))
                .unwrap_or(0);
            flight.ended_at_us = Some(heard.max(logged).max(flight.armed_at_us));
            flight.end = FlightEnd::Interrupted;
            // Its moving time died with the process, so it has none.
            flight.moving_us = None;
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
        Ok(self.flight_dto(ended))
    }

    /// `flight/uploadLog`: one ended flight's log to the account while every
    /// flight log is not being stored (2026-10-24). The uploader cuts it from
    /// the connection's log here and calls back once the account has it.
    pub(crate) fn ask_flight_log(
        &mut self,
        connection: crate::ConnectionId,
        id: &str,
    ) -> Result<uxduck_contracts::dto::flight::Flight, uxduck_contracts::errors::ApiError> {
        use uxduck_contracts::errors::{ApiError, ErrorKind};
        use uxduck_core::domain::flight::FlightLog;
        let mut flight = self
            .state
            .flights
            .get(id)
            .cloned()
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no flight {id}")))?;
        let refuse = |why: &str| Err(ApiError::new(ErrorKind::InvalidValue, why));
        if !flight.recorded_here || flight.link_id.is_none() {
            return refuse("Its telemetry log is on the computer that recorded it.");
        }
        if flight.end == FlightEnd::Open {
            return refuse("Upload its telemetry log once the flight has ended.");
        }
        if flight.simulated && !self.may_sync_simulated() {
            return refuse(SIMULATED_ON_PAID);
        }
        if flight.log == FlightLog::None {
            flight.log = FlightLog::Asked;
            flight.version += 1;
            self.commit(
                connection,
                Change::FlightUpserted {
                    flight: flight.clone(),
                },
            );
        }
        Ok(self.flight_dto(&flight))
    }

    /// The flight logs asked for and not in the account yet: each one's
    /// connection and its time, for the uploader to cut. A simulated
    /// flight's waits for a plan that takes it (ADR-0046 B6).
    #[must_use]
    pub fn flight_logs_asked(&self) -> Vec<(String, String, u64, u64)> {
        let simulated = self.may_sync_simulated();
        self.state
            .flights
            .values()
            .filter(|f| f.log == uxduck_core::domain::flight::FlightLog::Asked)
            .filter(|f| simulated || !f.simulated)
            .filter_map(|f| {
                Some((
                    f.id.clone(),
                    f.link_id.clone()?,
                    f.armed_at_us,
                    f.ended_at_us?,
                ))
            })
            .collect()
    }

    /// Of the flight logs cut and waiting here, by flight id: those to hold,
    /// a simulated flight's until the plan takes it (ADR-0046 B6), and
    /// those no longer wanted, deleted from the account or whose flight is
    /// gone (B2), to let go. The rest go.
    #[must_use]
    pub fn flight_logs_waiting(&self, recordings: Vec<String>) -> (Vec<String>, Vec<String>) {
        let simulated = self.may_sync_simulated();
        let (wanted, unwanted): (Vec<String>, Vec<String>) =
            recordings.into_iter().partition(|id| {
                self.state
                    .flights
                    .get(id)
                    .is_some_and(|f| f.log == uxduck_core::domain::flight::FlightLog::Asked)
            });
        let held = wanted
            .into_iter()
            .filter(|id| !simulated && self.state.flights.get(id).is_some_and(|f| f.simulated))
            .collect();
        (held, unwanted)
    }

    /// A flight's log is all in the account. Not one that was not asked for:
    /// any recording's chunks come here.
    pub fn flight_log_stored(&mut self, id: &str) {
        let Some(mut flight) = self.state.flights.get(id).cloned() else {
            return;
        };
        if flight.log != uxduck_core::domain::flight::FlightLog::Asked {
            return;
        }
        flight.log = uxduck_core::domain::flight::FlightLog::Stored;
        flight.version += 1;
        self.commit_as(flight_actor(), Change::FlightUpserted { flight });
    }

    /// A flight's log asked for, but its connection's logs here hold
    /// nothing of its time (deleted to make room, or never recorded).
    pub fn flight_log_missing(&mut self, id: &str) {
        let Some(mut flight) = self.state.flights.get(id).cloned() else {
            return;
        };
        if flight.log != uxduck_core::domain::flight::FlightLog::Asked {
            return;
        }
        flight.log = uxduck_core::domain::flight::FlightLog::None;
        flight.version += 1;
        let (vehicle, name) = (flight.vehicle_id.clone(), flight.vehicle_name.clone());
        self.commit_as(flight_actor(), Change::FlightUpserted { flight });
        self.raise(crate::commands::Raise {
            severity: uxduck_core::domain::notification::Severity::Info,
            kind: uxduck_core::domain::notification::NotificationKind::Transient,
            subject: uxduck_core::domain::notification::Subject::Vehicle { id: vehicle },
            title: format!("{name}'s telemetry log is not on this computer"),
            body: Some("Its time is not in the logs kept here, so nothing was sent.".into()),
            actions: Vec::new(),
            condition: None,
            report: None,
        });
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
    /// A simulation stopped or started again while its vehicle was armed:
    /// its flight ends now, interrupted, with its time in the air so far.
    /// A simulation stopped is gone for good, unlike a real vehicle out of
    /// contact, whose flight never ends by itself (maintainer, 2026-10-29).
    pub(crate) fn end_flights_on_simulation(&mut self, link_id: &str) {
        let now = self.clock.now_us();
        let on_link: Vec<String> = self
            .state
            .vehicles
            .values()
            .filter(|v| v.link_id == link_id)
            .map(|v| v.id.clone())
            .collect();
        for id in on_link {
            self.end_flights_of(&id, now);
        }
    }

    /// A vehicle gone for good (its simulation stopped, or deleted): no
    /// flight of it can still be flying, so each open one ends now,
    /// interrupted. One with no timer, as a flight kept from before UXDuck
    /// last started, ends when the vehicle was last heard.
    pub(crate) fn end_flights_of(&mut self, vehicle_id: &str, now: u64) {
        if self.open_flight(vehicle_id).is_some() {
            self.end_flight(vehicle_id, FlightEnd::Interrupted, now);
            if let Some(timer) = self.flight_times.0.get_mut(vehicle_id) {
                if let Some(at) = timer.armed_at_us.take() {
                    timer.armed_us = now.saturating_sub(at);
                }
                if let Some((from, until)) = timer.hop.end(now) {
                    timer.air_us += until.saturating_sub(from);
                }
            }
        }
        let heard = self
            .state
            .vehicles
            .get(vehicle_id)
            .map(|v| v.last_heartbeat_us);
        let left: Vec<Flight> = self
            .state
            .flights
            .values()
            .filter(|f| f.vehicle_id == vehicle_id && f.end == FlightEnd::Open)
            .cloned()
            .collect();
        for mut flight in left {
            let at = heard.unwrap_or(flight.armed_at_us).min(now);
            flight.ended_at_us = Some(at.max(flight.armed_at_us));
            flight.end = FlightEnd::Interrupted;
            flight.contact_lost_at_us = None;
            flight.moving_us = None;
            flight.version += 1;
            self.commit_as(flight_actor(), Change::FlightUpserted { flight });
        }
    }

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
            if let Some((from, until)) = timer.hop.end(now) {
                timer.air_us += until.saturating_sub(from);
            }
            timer.sent = None;
        }
        self.review_flight_time(vehicle_id);
    }

    /// Whether a vehicle is off the ground now: the autopilot's word, else
    /// its height above home, with `was` for the band between the two lines.
    /// As it is said this moment: a [`Hop`] is what lasts through a flicker.
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

    /// Whether the vehicle is past its launch (ADR-0038 K23): in the air
    /// for one that flies, and armed for one that does not, which has no
    /// leaving the ground to tell by. Until then its checks are read live
    /// and hold a launch. Armed alone for a motor test is not launched.
    /// In the air as its hop has it, so "on the ground" for a moment in
    /// flight does not read its checks live or hold a mission's start.
    pub(crate) fn launched(&self, vehicle: &uxduck_core::domain::vehicle::Vehicle) -> bool {
        if !vehicle.armed {
            return false;
        }
        if !flies(&vehicle.vehicle_type) {
            return true;
        }
        let flying = self
            .flight_times
            .0
            .get(&vehicle.id)
            .is_some_and(|t| t.hop.flying());
        flying || self.off_the_ground(&vehicle.id, false)
    }

    /// When the vehicle armed, while it is armed.
    pub(crate) fn armed_since_us(&self, vehicle_id: &str) -> Option<u64> {
        self.flight_times
            .0
            .get(vehicle_id)
            .and_then(|t| t.armed_at_us)
    }

    /// Telemetry arrived, or the vehicle armed or disarmed: start or stop
    /// its time in the air, and send its times when a second has passed.
    #[allow(
        clippy::too_many_lines,
        reason = "one pass over a vehicle's flight state"
    )]
    pub(crate) fn review_flight_time(&mut self, vehicle_id: &str) {
        let Some(vehicle) = self.vehicle(vehicle_id) else {
            return;
        };
        let (id, armed, aerial) = (
            vehicle.id.clone(),
            vehicle.armed,
            flies(&vehicle.vehicle_type),
        );
        // Armed by itself for a motor test is no flight (S188).
        let armed = armed && !self.armed_by_motor_test(&id);
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
            let reached = self
                .telemetry(&id)
                .and_then(|t| t.items.iter().find(|i| i.key == "mission.reached"))
                .and_then(|i| i.value.map(|v| (v, i.time_us)));
            if let Some(timer) = self.flight_times.0.get_mut(&id)
                && timer.flight.is_some()
            {
                timer.max_height_m = higher(timer.max_height_m, height);
                timer.max_speed_mps = higher(timer.max_speed_mps, speed);
                let moving = speed.is_some_and(|s| s > MOVING_ABOVE_MPS);
                match (moving, timer.moving_since_us) {
                    (true, None) => timer.moving_since_us = Some(now),
                    (false, Some(at)) => {
                        timer.moving_us += now.saturating_sub(at);
                        timer.moving_since_us = None;
                    }
                    _ => {}
                }
                // MISSION_CURRENT: state 3 active, 5 complete; mode 1 in a mission mode.
                match (mission_state, mission_mode) {
                    (Some(3), _) | (_, Some(1)) => timer.mission_flown = true,
                    _ => {}
                }
                if mission_state == Some(5) && timer.mission_flown {
                    timer.mission_complete = true;
                }
                // A step reached this flight: not the one it had reached
                // before it armed.
                if !timer.reached_looked {
                    timer.reached_looked = true;
                    timer.reached_before = reached;
                }
                if reached != timer.reached_before {
                    timer.reached = reached.map(|(item, _)| item);
                }
                if let Some(mode) = mode
                    && timer.modes.last() != Some(&mode)
                    && !timer.modes.contains(&mode)
                {
                    timer.modes.push(mode);
                }
            }
        }
        let was_up = self.flight_times.0.get(&id).is_some_and(|t| t.hop.up());
        let up = armed && aerial && self.off_the_ground(&id, was_up);
        let Some(timer) = self.flight_times.0.get_mut(&id) else {
            return;
        };
        let lifted = match timer.hop.observe(up, now) {
            Some(HopChange::Lifted) => true,
            Some(HopChange::Landed { from_us, until_us }) => {
                timer.air_us += until_us.saturating_sub(from_us);
                false
            }
            None => false,
        };
        // It left the ground: what its launch went past, and a warning
        // when UXDuck did not launch it (ADR-0038 K23).
        if lifted {
            self.checklist_lifted_off(&id);
        }
        let Some(timer) = self.flight_times.0.get_mut(&id) else {
            return;
        };
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

/// Totals over the listed entries: time in the air, armed and moving, real
/// and simulated, all-time and since `since`. Moving time adds up only the
/// entries that kept it, and the rest are counted apart (FB-228).
fn logbook_totals(
    flights: &[&Flight],
    since: Option<u64>,
) -> uxduck_contracts::dto::flight::FlightTotals {
    let mut totals = uxduck_contracts::dto::flight::FlightTotals {
        count: u32::try_from(flights.len()).unwrap_or(u32::MAX),
        ..Default::default()
    };
    totals.vehicles = u32::try_from(
        flights
            .iter()
            .map(|f| f.vehicle_id.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
    )
    .unwrap_or(u32::MAX);
    for flight in flights {
        let recent = since.is_some_and(|s| flight.armed_at_us >= s);
        let t = &mut totals;
        let (all, lately, armed, armed_lately, moving, moving_lately) = if flight.simulated {
            (
                &mut t.simulated_air_us,
                &mut t.simulated_air_us_since,
                &mut t.simulated_armed_us,
                &mut t.simulated_armed_us_since,
                &mut t.simulated_moving_us,
                &mut t.simulated_moving_us_since,
            )
        } else {
            (
                &mut t.real_air_us,
                &mut t.real_air_us_since,
                &mut t.real_armed_us,
                &mut t.real_armed_us_since,
                &mut t.real_moving_us,
                &mut t.real_moving_us_since,
            )
        };
        let armed_us = armed_for_us(flight);
        *all += flight.air_us;
        *armed += armed_us;
        *moving += flight.moving_us.unwrap_or(0);
        if recent {
            *lately += flight.air_us;
            *armed_lately += armed_us;
            *moving_lately += flight.moving_us.unwrap_or(0);
        }
        if flight.moving_us.is_none() {
            totals.without_moving_time += 1;
        }
        if VehicleClass::of(&flight.vehicle_type).never_flies() {
            totals.grounded += 1;
        }
    }
    totals
}

#[cfg(test)]
mod hop_tests {
    use super::{Hop, HopChange, LANDED_AFTER_US};

    const S: u64 = 1_000_000;

    #[test]
    fn a_moment_on_the_ground_in_flight_is_the_same_hop_and_counts() {
        let mut hop = Hop::default();
        assert_eq!(hop.observe(false, 0), None);
        assert_eq!(hop.observe(true, 10 * S), Some(HopChange::Lifted));
        // A land detector that flickers: on the ground every other second.
        for second in 11..=40 {
            let up = second % 2 == 0;
            assert_eq!(hop.observe(up, second * S), None, "{second}");
            assert!(hop.flying(), "{second}");
            assert_eq!(hop.up(), up, "{second}");
        }
        assert_eq!(hop.lasted_us(40 * S), 30 * S);
        // Down for good: the time stops where it came down, and that is
        // when it landed, once it has stayed down.
        assert_eq!(hop.observe(false, 41 * S), None);
        assert_eq!(hop.lasted_us(43 * S), 31 * S);
        assert_eq!(hop.observe(false, 41 * S + LANDED_AFTER_US - 1), None);
        assert_eq!(
            hop.observe(false, 41 * S + LANDED_AFTER_US),
            Some(HopChange::Landed {
                from_us: 10 * S,
                until_us: 41 * S
            })
        );
        assert!(!hop.flying());
        assert_eq!(hop.lasted_us(50 * S), 0);
        // Up again is another hop.
        assert_eq!(hop.observe(true, 50 * S), Some(HopChange::Lifted));
    }

    #[test]
    fn a_hop_that_ends_with_its_flight_ends_where_it_came_down() {
        let mut hop = Hop::default();
        assert_eq!(hop.end(S), None);
        hop.observe(true, 5 * S);
        assert_eq!(hop.end(9 * S), Some((5 * S, 9 * S)), "still up");
        hop.observe(true, 20 * S);
        hop.observe(false, 30 * S);
        assert_eq!(hop.end(31 * S), Some((20 * S, 30 * S)), "down a second");
        assert!(!hop.flying());
        // Ended at a time before it came down, as a flight whose contact
        // was lost is: no later than that.
        hop.observe(true, 40 * S);
        hop.observe(false, 50 * S);
        assert_eq!(hop.end(45 * S), Some((40 * S, 45 * S)));
    }
}

#[cfg(test)]
mod page_cursor_tests {
    use super::parse_page_cursor;

    #[test]
    fn a_page_cursor_is_when_its_flight_armed_and_its_id() {
        assert_eq!(parse_page_cursor("12:fl:a"), Some((12, "fl:a".to_owned())));
        for bad in [
            "",
            ":",
            "12:",
            ":fl",
            "x:fl",
            "-1:fl",
            "1.5:fl",
            "99999999999999999999:fl",
        ] {
            assert_eq!(parse_page_cursor(bad), None, "{bad}");
        }
    }
}

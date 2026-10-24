//! Other aircraft near the vehicles (S168, ADR-0034). Each source's reports
//! are kept apart and checked by themselves, so one vehicle's faulty
//! receiver is refused, counted and paused without touching the others.
//! The picture merges the sources a person turned on, by address, freshest
//! first. Live state: kept here only, never journaled; `traffic/status`
//! answers it and `traffic/changed` follows.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use uxduck_contracts::dto::Empty;
use uxduck_contracts::dto::plugin::{PluginTraffic, PluginTrafficStatus};
use uxduck_contracts::dto::traffic::{
    TrafficAircraft, TrafficAltitudeFrame, TrafficCategory as CategoryDto, TrafficOwnMatch,
    TrafficProtocol, TrafficRefusal, TrafficRefusalReason as Reason, TrafficSource,
    TrafficSourceKind, TrafficSourceState, TrafficStatus, VehicleTrafficParams,
};
use uxduck_contracts::dto::vehicle::Vehicle as VehicleDto;
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::{ClientRequest, ServerNotification};
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::link::{LinkKind, LinkStatus};
use uxduck_core::domain::notification::{NotificationKind, Severity, Subject};
use uxduck_core::domain::telemetry::AltitudeFrame as TelemetryFrame;
use uxduck_core::domain::traffic::{AltitudeFrame, TrafficCategory, TrafficReport};

use crate::commands::Raise;
use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};
use uxduck_core::events::Change;

/// Not heard for this long, an aircraft is drawn grey.
pub const STALE_US: u64 = 10_000_000;
/// Not heard for this long, an aircraft is gone, and a source is quiet.
pub const GONE_US: u64 = 60_000_000;
/// The most aircraft one source may give.
pub const MAX_PER_SOURCE: usize = 500;
/// More reports than this in a second pause a source.
pub const FLOOD_PER_SECOND: u32 = 2_000;
/// How long a flooding source is ignored.
pub const PAUSE_US: u64 = 30_000_000;
/// Faster than any aircraft, with room for a receiver's jitter.
const MAX_SPEED_MPS: f64 = 600.0;
const JITTER_M: f64 = 1_000.0;
/// `traffic/changed` at most this often.
const PUSH_EVERY_US: u64 = 250_000;

#[derive(Debug, Default)]
pub(crate) struct Traffic {
    sources: BTreeMap<String, Source>,
    /// Connections that asked, which hear `traffic/changed`.
    watchers: BTreeSet<ConnectionId>,
    /// Something changed since the last push.
    dirty: bool,
    last_push_us: u64,
    /// Warnings raised (S174), by vehicle id and aircraft address.
    alerts: BTreeSet<(String, String)>,
}

/// Whose a source is: a vehicle's receiver, or a plugin (S170).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Owner {
    Vehicle(String),
    Plugin(String),
    /// A link carrying a receiver's traffic (S172).
    Receiver(String),
}

impl Default for Owner {
    fn default() -> Self {
        Self::Vehicle(String::new())
    }
}

impl Owner {
    fn source_id(&self) -> String {
        match self {
            Self::Vehicle(id) => format!("vehicle:{id}"),
            Self::Plugin(id) => format!("plugin:{id}"),
            Self::Receiver(id) => format!("receiver:{id}"),
        }
    }
}

#[derive(Debug, Default)]
struct Source {
    owner: Owner,
    /// How it says it is doing (a plugin's, S171).
    status: Option<PluginTrafficStatus>,
    records: BTreeMap<u32, TrafficReport>,
    last_heard_us: Option<u64>,
    refused: BTreeMap<Reason, u64>,
    window_start_us: u64,
    window_count: u32,
    paused_until_us: Option<u64>,
}

fn alert_condition(vehicle_id: &str, address: &str) -> String {
    format!("traffic-near:{vehicle_id}:{address}")
}

fn flood_condition(id: &str) -> String {
    format!("traffic-flood:{id}")
}

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    operator: bool,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    match request {
        ClientRequest::TrafficStatus(Empty {}) => {
            if !granted.contains(&Capability::Read) {
                return Err(ApiError::new(
                    ErrorKind::CapabilityDenied,
                    Capability::Read.denied_message(),
                ));
            }
            engine.traffic.watchers.insert(connection);
            to_value(&engine.traffic_status())
        }
        ClientRequest::VehicleTraffic(VehicleTrafficParams { id, on }) => {
            if !operator {
                return Err(ApiError::new(
                    ErrorKind::CapabilityDenied,
                    "only a person can turn a vehicle's traffic on or off",
                ));
            }
            let mut vehicle = engine
                .vehicle(&id)
                .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no vehicle {id}")))?
                .clone();
            if vehicle.traffic != on {
                vehicle.traffic = on;
                vehicle.version += 1;
                engine.commit(
                    connection,
                    Change::VehicleUpserted {
                        vehicle: vehicle.clone(),
                    },
                );
                engine.traffic.dirty = true;
                engine.push_traffic(true);
            }
            to_value(&VehicleDto::from(&vehicle))
        }
        _ => Err(ApiError::new(ErrorKind::Internal, "not a traffic method")),
    }
}

impl Engine {
    /// An aircraft a vehicle's receiver heard (`ADSB_VEHICLE`).
    pub fn vehicle_traffic(&mut self, vehicle_id: &str, report: TrafficReport) {
        self.observe_traffic(&Owner::Vehicle(vehicle_id.to_owned()), Ok(report));
        self.push_traffic(false);
    }

    /// Aircraft a plugin reported (S170): each checked as a vehicle's are,
    /// an address that is not six hexadecimal digits refused.
    pub(crate) fn plugin_traffic(&mut self, plugin_id: &str, reports: Vec<PluginTraffic>) {
        if reports.is_empty() {
            return;
        }
        let owner = Owner::Plugin(plugin_id.to_owned());
        let now = self.clock.now_us();
        for report in reports {
            self.observe_traffic(&owner, plugin_report(report, now));
        }
        self.push_traffic(false);
    }

    /// How a plugin's traffic source says it is doing (S171).
    pub(crate) fn plugin_traffic_status(&mut self, plugin_id: &str, status: PluginTrafficStatus) {
        let owner = Owner::Plugin(plugin_id.to_owned());
        let source = self.traffic.sources.entry(owner.source_id()).or_default();
        source.owner = owner;
        if source.status.as_ref() != Some(&status) {
            source.status = Some(status);
            self.traffic.dirty = true;
            self.push_traffic(false);
        }
    }

    /// Aircraft a receiver on the network reported (S172), on its link.
    pub fn receiver_traffic(&mut self, link_id: &str, reports: Vec<TrafficReport>) {
        let owner = Owner::Receiver(link_id.to_owned());
        for report in reports {
            self.observe_traffic(&owner, Ok(report));
        }
        self.push_traffic(false);
    }

    /// A source's on or off changed elsewhere, such as its plugin (S170).
    pub(crate) fn traffic_sources_changed(&mut self) {
        self.traffic.dirty = true;
        self.push_traffic(true);
    }

    fn observe_traffic(&mut self, owner: &Owner, report: Result<TrafficReport, Reason>) {
        let now = self.clock.now_us();
        let id = owner.source_id();
        let source = self.traffic.sources.entry(id.clone()).or_default();
        source.owner = owner.clone();
        source.last_heard_us = Some(now);
        if now.saturating_sub(source.window_start_us) >= 1_000_000 {
            source.window_start_us = now;
            source.window_count = 0;
        }
        source.window_count += 1;
        let resumed = source.paused_until_us.is_some_and(|until| now >= until);
        if resumed {
            source.paused_until_us = None;
        }
        let flooding = source.paused_until_us.is_none() && source.window_count > FLOOD_PER_SECOND;
        if flooding {
            source.paused_until_us = Some(now + PAUSE_US);
            source.records.clear();
        }
        let checked = match report {
            _ if source.paused_until_us.is_some() => Err(Reason::Paused),
            Ok(report) => match refusal(source, &report) {
                Some(reason) => Err(reason),
                None => Ok(report),
            },
            Err(reason) => Err(reason),
        };
        match checked {
            Err(reason) => *source.refused.entry(reason).or_default() += 1,
            Ok(report) => {
                source.records.insert(report.address, report);
            }
        }
        if resumed {
            self.resolve(&flood_condition(&id));
        }
        if flooding {
            self.raise_flood(&id, owner);
        }
        self.traffic.dirty = true;
    }

    fn source_on(&self, owner: &Owner) -> bool {
        match owner {
            Owner::Vehicle(id) => self.state.vehicles.get(id).is_some_and(|v| v.traffic),
            Owner::Plugin(id) => self.state.plugins.get(id).is_some_and(|p| p.enabled),
            Owner::Receiver(id) => self.state.links.get(id).is_some_and(|l| !l.status.is_off()),
        }
    }

    /// How a receiver's link is doing, in its row (S172): nothing while up.
    fn receiver_note(&self, link_id: &str) -> Option<(String, bool)> {
        let link = self.state.links.get(link_id)?;
        let detail = link
            .status_detail
            .as_deref()
            .map_or_else(String::new, |d| format!(": {d}"));
        match link.status {
            LinkStatus::Up => None,
            LinkStatus::Configured => Some(("Connecting".into(), false)),
            LinkStatus::Disabled | LinkStatus::Stopped | LinkStatus::DisabledByLoopDetection => {
                None
            }
            // A stick's decoder says what is wrong in its own words (S173).
            LinkStatus::Down if matches!(link.kind, LinkKind::UsbReceiver {}) => Some((
                link.status_detail
                    .clone()
                    .unwrap_or_else(|| "The receiver stick stopped".into()),
                true,
            )),
            LinkStatus::Down => Some((format!("Cannot reach the receiver{detail}"), true)),
            LinkStatus::PortInUse => Some((format!("Its port is in use{detail}"), true)),
        }
    }

    fn source_exists(&self, owner: &Owner) -> bool {
        match owner {
            Owner::Vehicle(id) => self.state.vehicles.contains_key(id),
            Owner::Plugin(id) => self.state.plugins.contains_key(id),
            Owner::Receiver(id) => self.state.links.contains_key(id),
        }
    }

    /// Forgets aircraft not heard for a minute, ends pauses, and tells
    /// watchers what aged. Called once a second.
    pub fn expire_traffic(&mut self, now_us: u64) {
        let mut resumed = Vec::new();
        for (id, source) in &mut self.traffic.sources {
            let before = source.records.len();
            source
                .records
                .retain(|_, r| now_us.saturating_sub(r.heard_us) < GONE_US);
            if source.records.len() != before || !source.records.is_empty() {
                // Aircraft turn grey as they age, so any left is news.
                self.traffic.dirty = true;
            }
            if source.paused_until_us.is_some_and(|until| now_us >= until) {
                source.paused_until_us = None;
                resumed.push(id.clone());
                self.traffic.dirty = true;
            }
        }
        for id in resumed {
            self.resolve(&flood_condition(&id));
        }
        self.push_traffic(true);
    }

    pub(crate) fn traffic_disconnected(&mut self, connection: ConnectionId) {
        self.traffic.watchers.remove(&connection);
    }

    /// The picture and every source, as `traffic/status` answers it.
    #[must_use]
    #[allow(
        clippy::too_many_lines,
        reason = "the picture, then each kind of source"
    )]
    pub fn traffic_status(&self) -> TrafficStatus {
        let now = self.clock.now_us();
        let mut merged: BTreeMap<u32, (&TrafficReport, Vec<String>)> = BTreeMap::new();
        for (id, source) in &self.traffic.sources {
            if source.paused_until_us.is_some() || !self.source_on(&source.owner) {
                continue;
            }
            for (address, report) in &source.records {
                if now.saturating_sub(report.heard_us) >= GONE_US {
                    continue;
                }
                let entry = merged
                    .entry(*address)
                    .or_insert_with(|| (report, Vec::new()));
                if report.heard_us > entry.0.heard_us {
                    entry.0 = report;
                }
                entry.1.push(id.clone());
            }
        }
        let ours = self.own_vehicles();
        let aircraft = merged
            .into_values()
            .map(|(report, sources)| {
                let mut seen = aircraft(report, sources, now);
                if let Some((vehicle_id, how)) = own(report, &ours) {
                    seen.vehicle_id = Some(vehicle_id);
                    seen.own = Some(how);
                }
                seen
            })
            .collect();
        let mut sources: Vec<TrafficSource> = self
            .traffic
            .sources
            .iter()
            .filter(|(_, source)| self.source_exists(&source.owner))
            .map(|(id, source)| TrafficSource {
                id: id.clone(),
                kind: match source.owner {
                    Owner::Vehicle(_) => TrafficSourceKind::Vehicle,
                    Owner::Plugin(_) => TrafficSourceKind::Plugin,
                    Owner::Receiver(_) => TrafficSourceKind::Receiver,
                },
                vehicle_id: match &source.owner {
                    Owner::Vehicle(id) => Some(id.clone()),
                    _ => None,
                },
                plugin_id: match &source.owner {
                    Owner::Plugin(id) => Some(id.clone()),
                    _ => None,
                },
                link_id: match &source.owner {
                    Owner::Receiver(id) => Some(id.clone()),
                    _ => None,
                },
                on: self.source_on(&source.owner),
                state: if source.paused_until_us.is_some() {
                    TrafficSourceState::Paused
                } else if source
                    .last_heard_us
                    .is_some_and(|at| now.saturating_sub(at) < GONE_US)
                {
                    TrafficSourceState::Hearing
                } else {
                    TrafficSourceState::Quiet
                },
                aircraft: u32::try_from(
                    source
                        .records
                        .values()
                        .filter(|r| now.saturating_sub(r.heard_us) < GONE_US)
                        .count(),
                )
                .unwrap_or(u32::MAX),
                last_heard_at_ms: source.last_heard_us.map(|us| us / 1000),
                refused: source
                    .refused
                    .iter()
                    .map(|(reason, count)| TrafficRefusal {
                        reason: *reason,
                        count: *count,
                    })
                    .collect(),
                note: match &source.owner {
                    Owner::Receiver(id) => self.receiver_note(id).map(|(note, _)| note),
                    _ => source.status.as_ref().and_then(|s| s.note.clone()),
                },
                problem: match &source.owner {
                    Owner::Receiver(id) => self.receiver_note(id).is_some_and(|(_, p)| p),
                    _ => source.status.as_ref().is_some_and(|s| s.problem),
                },
                credit: source.status.as_ref().and_then(|s| s.credit.clone()),
            })
            .collect();
        // A plugin that is a traffic source is listed on or off (S171), with
        // what it says it does while it has said nothing yet.
        for plugin in self.state.plugins.values() {
            let id = Owner::Plugin(plugin.id.clone()).source_id();
            if plugin.traffic_source && !self.traffic.sources.contains_key(&id) {
                sources.push(TrafficSource {
                    id,
                    kind: TrafficSourceKind::Plugin,
                    vehicle_id: None,
                    plugin_id: Some(plugin.id.clone()),
                    link_id: None,
                    on: plugin.enabled,
                    state: TrafficSourceState::Quiet,
                    aircraft: 0,
                    last_heard_at_ms: None,
                    refused: Vec::new(),
                    note: plugin.description.clone(),
                    problem: false,
                    credit: None,
                });
            }
        }
        // A receiver's link is a source from the moment it is added (S172).
        for link in self.state.links.values() {
            let id = Owner::Receiver(link.id.clone()).source_id();
            if !link.carries.is_mavlink() && !self.traffic.sources.contains_key(&id) {
                let note = self.receiver_note(&link.id);
                sources.push(TrafficSource {
                    id,
                    kind: TrafficSourceKind::Receiver,
                    vehicle_id: None,
                    plugin_id: None,
                    link_id: Some(link.id.clone()),
                    on: !link.status.is_off(),
                    state: TrafficSourceState::Quiet,
                    aircraft: 0,
                    last_heard_at_ms: None,
                    refused: Vec::new(),
                    note: note.as_ref().map(|(n, _)| n.clone()),
                    problem: note.is_some_and(|(_, p)| p),
                    credit: None,
                });
            }
        }
        // A vehicle turned on but not heard from yet is a source too.
        for vehicle in self.state.vehicles.values() {
            let id = Owner::Vehicle(vehicle.id.clone()).source_id();
            if vehicle.traffic && !self.traffic.sources.contains_key(&id) {
                sources.push(TrafficSource {
                    id,
                    kind: TrafficSourceKind::Vehicle,
                    vehicle_id: Some(vehicle.id.clone()),
                    plugin_id: None,
                    link_id: None,
                    on: true,
                    state: TrafficSourceState::Quiet,
                    aircraft: 0,
                    last_heard_at_ms: None,
                    refused: Vec::new(),
                    note: None,
                    problem: false,
                    credit: None,
                });
            }
        }
        TrafficStatus { aircraft, sources }
    }

    /// Our vehicles as a receiver may hear them: the address they send
    /// with (`ADSB_ICAO_ID`) and where they are.
    fn own_vehicles(&self) -> Vec<Ours> {
        self.state
            .vehicles
            .values()
            .map(|vehicle| {
                let address = self
                    .state
                    .parameters
                    .get(&format!("{}/ADSB_ICAO_ID", vehicle.id))
                    .map(|p| p.value)
                    .filter(|v| *v >= 1.0 && *v <= f64::from(0x00FF_FFFF_u32))
                    .map(|v| {
                        #[allow(
                            clippy::cast_possible_truncation,
                            clippy::cast_sign_loss,
                            reason = "checked to be a 24-bit address"
                        )]
                        let address = v as u32;
                        address
                    });
                let at = self.telemetry.get(&vehicle.id).and_then(|t| {
                    let p = t
                        .positions
                        .iter()
                        .find(|p| p.source == "fused")
                        .or_else(|| t.positions.first())?;
                    let amsl = p
                        .altitudes
                        .iter()
                        .find(|a| a.frame == TelemetryFrame::Amsl)?
                        .meters;
                    Some((p.latitude_deg, p.longitude_deg, amsl))
                });
                Ours {
                    vehicle_id: vehicle.id.clone(),
                    address,
                    at,
                }
            })
            .collect()
    }

    /// Tells watchers, at most four times a second unless `now`.
    fn push_traffic(&mut self, force: bool) {
        let now = self.clock.now_us();
        if !self.traffic.dirty
            || (!force && now.saturating_sub(self.traffic.last_push_us) < PUSH_EVERY_US)
        {
            return;
        }
        self.traffic.dirty = false;
        self.traffic.last_push_us = now;
        let status = self.traffic_status();
        self.review_alerts(&status);
        if self.traffic.watchers.is_empty() {
            return;
        }
        let frame = crate::rpc::router::notification(&ServerNotification::TrafficChanged(status));
        let watchers: Vec<ConnectionId> = self.traffic.watchers.iter().copied().collect();
        for id in watchers {
            self.send(id, &frame);
        }
    }

    /// Warns of traffic within Settings › Airspace's distances of an armed
    /// vehicle (S174), and clears the warning once it is a quarter beyond
    /// them, gone or not heard lately. Never for our own vehicles.
    fn review_alerts(&mut self, status: &TrafficStatus) {
        let (across, up) = (
            f64::from(self.state.settings.traffic_alert_m),
            f64::from(self.state.settings.traffic_alert_up_m),
        );
        let mut near: BTreeMap<(String, String), (String, f64, f64)> = BTreeMap::new();
        let mut still: BTreeSet<(String, String)> = BTreeSet::new();
        for ours in self.own_vehicles() {
            let armed = self
                .state
                .vehicles
                .get(&ours.vehicle_id)
                .is_some_and(|v| v.armed);
            let Some((at_lat, at_lon, amsl)) = ours.at.filter(|_| armed) else {
                continue;
            };
            for aircraft in &status.aircraft {
                let (Some(plane_lat), Some(plane_lon), Some(alt)) = (
                    aircraft.latitude_deg,
                    aircraft.longitude_deg,
                    aircraft.altitude_m,
                ) else {
                    continue;
                };
                if aircraft.vehicle_id.is_some() || aircraft.stale {
                    continue;
                }
                let distance = metres_between((at_lat, at_lon), (plane_lat, plane_lon));
                let above = alt - amsl;
                let key = (ours.vehicle_id.clone(), aircraft.address.clone());
                if distance <= across && above.abs() <= up {
                    let name = aircraft
                        .callsign
                        .clone()
                        .unwrap_or_else(|| aircraft.address.clone());
                    near.insert(key, (name, distance, above));
                } else if distance <= across * 1.25 && above.abs() <= up * 1.25 {
                    still.insert(key);
                }
            }
        }
        let cleared: Vec<(String, String)> = self
            .traffic
            .alerts
            .iter()
            .filter(|key| !near.contains_key(*key) && !still.contains(*key))
            .cloned()
            .collect();
        for key in cleared {
            self.traffic.alerts.remove(&key);
            self.resolve(&alert_condition(&key.0, &key.1));
        }
        for (key, (name, distance, above)) in near {
            if !self.traffic.alerts.insert(key.clone()) {
                continue;
            }
            let vehicle = self
                .state
                .vehicles
                .get(&key.0)
                .map_or_else(|| "the vehicle".to_owned(), |v| v.name.clone());
            let height = if above.abs() < 15.0 {
                "at the same height".to_owned()
            } else {
                format!(
                    "{:.0} m {}",
                    above.abs(),
                    if above > 0.0 { "above" } else { "below" }
                )
            };
            let away = if distance < 1_000.0 {
                format!("{distance:.0} m")
            } else {
                format!("{:.1} km", distance / 1_000.0)
            };
            self.raise(Raise {
                severity: Severity::Warning,
                kind: NotificationKind::Persistent,
                subject: Subject::Vehicle { id: key.0.clone() },
                title: format!("{name} near {vehicle}"),
                body: Some(format!(
                    "{away} away, {height}. Traffic is never complete: look out as well."
                )),
                actions: Vec::new(),
                condition: Some(alert_condition(&key.0, &key.1)),
                report: None,
            });
        }
    }

    fn raise_flood(&mut self, source_id: &str, owner: &Owner) {
        let (name, subject, what) = match owner {
            Owner::Vehicle(id) => (
                self.state
                    .vehicles
                    .get(id)
                    .map_or_else(|| "A vehicle".to_owned(), |v| v.name.clone()),
                Subject::Vehicle { id: id.clone() },
                "Its ADS-B receiver",
            ),
            Owner::Plugin(id) => (
                self.state
                    .plugins
                    .get(id)
                    .map_or_else(|| id.clone(), |p| p.name.clone()),
                Subject::System,
                "It",
            ),
            Owner::Receiver(id) => (
                self.state
                    .links
                    .get(id)
                    .map_or_else(|| "A receiver".to_owned(), |l| l.name.clone()),
                Subject::Link { id: id.clone() },
                "The receiver",
            ),
        };
        self.raise(Raise {
            severity: Severity::Warning,
            kind: NotificationKind::Persistent,
            subject,
            title: format!("{name}'s traffic is paused"),
            body: Some(format!(
                "{what} sent more than {FLOOD_PER_SECOND} reports in a second. UXDuck ignores it for {} seconds; the other sources carry on.",
                PAUSE_US / 1_000_000
            )),
            actions: Vec::new(),
            condition: Some(flood_condition(source_id)),
            report: Some("traffic-flood".into()),
        });
    }
}

/// One of our vehicles, for telling it apart from traffic.
struct Ours {
    vehicle_id: String,
    address: Option<u32>,
    /// Latitude, longitude and metres above sea level.
    at: Option<(f64, f64, f64)>,
}

/// Across, and up or down, within which an aircraft is probably the vehicle.
const NEARBY_M: f64 = 30.0;
const NEARBY_UP_M: f64 = 15.0;
/// Up or down for a pressure altitude, which differs from GNSS by the weather.
const NEARBY_UP_PRESSURE_M: f64 = 50.0;

/// Which of our vehicles an aircraft is, if it is one (S169, ADR-0034).
fn own(report: &TrafficReport, ours: &[Ours]) -> Option<(String, TrafficOwnMatch)> {
    let address = report.address & 0x00FF_FFFF;
    if let Some(vehicle) = ours.iter().find(|v| v.address == Some(address)) {
        return Some((vehicle.vehicle_id.clone(), TrafficOwnMatch::Address));
    }
    let (position, altitude) = (report.position?, report.altitude_m?);
    let up = if report.altitude_frame == Some(AltitudeFrame::Pressure) {
        NEARBY_UP_PRESSURE_M
    } else {
        NEARBY_UP_M
    };
    ours.iter()
        .find(|v| {
            v.at.is_some_and(|(lat, lon, amsl)| {
                metres_between(position, (lat, lon)) <= NEARBY_M && (altitude - amsl).abs() <= up
            })
        })
        .map(|v| (v.vehicle_id.clone(), TrafficOwnMatch::Nearby))
}

/// A plugin's aircraft as a report (S170): heard `seenAgoS` before now.
fn plugin_report(report: PluginTraffic, now_us: u64) -> Result<TrafficReport, Reason> {
    let hex = report.address.trim();
    let address = (hex.len() == 6)
        .then(|| u32::from_str_radix(hex, 16).ok())
        .flatten()
        .ok_or(Reason::Address)?;
    let position = match (report.latitude_deg, report.longitude_deg) {
        (Some(lat), Some(lon)) => Some((lat, lon)),
        _ => None,
    };
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "seconds ago, clamped to a day"
    )]
    let ago_us = (report.seen_ago_s.unwrap_or(0.0).clamp(0.0, 86_400.0) * 1e6) as u64;
    Ok(TrafficReport {
        address,
        callsign: report
            .callsign
            .map(|c| c.trim().to_owned())
            .filter(|c| !c.is_empty()),
        squawk: report.squawk.as_deref().and_then(|s| s.trim().parse().ok()),
        category: core_category(report.category),
        protocol: match report.protocol {
            TrafficProtocol::Adsb => uxduck_core::domain::traffic::TrafficProtocol::Adsb,
            TrafficProtocol::Uat => uxduck_core::domain::traffic::TrafficProtocol::Uat,
        },
        position,
        altitude_m: report.altitude_m,
        altitude_frame: report.altitude_frame.map(|frame| match frame {
            TrafficAltitudeFrame::Pressure => AltitudeFrame::Pressure,
            TrafficAltitudeFrame::Geometric => AltitudeFrame::Geometric,
        }),
        track_deg: report.track_deg.map(|t| t.rem_euclid(360.0)),
        ground_speed_mps: report.ground_speed_mps,
        climb_mps: report.climb_mps,
        simulated: report.simulated,
        heard_us: now_us.saturating_sub(ago_us),
    })
}

fn core_category(category: CategoryDto) -> TrafficCategory {
    match category {
        CategoryDto::Unknown => TrafficCategory::Unknown,
        CategoryDto::Light => TrafficCategory::Light,
        CategoryDto::Small => TrafficCategory::Small,
        CategoryDto::Large => TrafficCategory::Large,
        CategoryDto::Heavy => TrafficCategory::Heavy,
        CategoryDto::Fast => TrafficCategory::Fast,
        CategoryDto::Rotorcraft => TrafficCategory::Rotorcraft,
        CategoryDto::Glider => TrafficCategory::Glider,
        CategoryDto::Balloon => TrafficCategory::Balloon,
        CategoryDto::Parachute => TrafficCategory::Parachute,
        CategoryDto::Ultralight => TrafficCategory::Ultralight,
        CategoryDto::Uav => TrafficCategory::Uav,
        CategoryDto::Space => TrafficCategory::Space,
        CategoryDto::Surface => TrafficCategory::Surface,
        CategoryDto::Obstacle => TrafficCategory::Obstacle,
    }
}

/// Why a source's report is refused, if it is (ADR-0034).
fn refusal(source: &Source, report: &TrafficReport) -> Option<Reason> {
    if let Some((lat, lon)) = report.position
        && (!lat.is_finite()
            || !lon.is_finite()
            || lat.abs() > 90.0
            || lon.abs() > 180.0
            || (lat.abs() < 1e-6 && lon.abs() < 1e-6))
    {
        return Some(Reason::Position);
    }
    if report
        .altitude_m
        .is_some_and(|m| !m.is_finite() || !(-500.0..=20_000.0).contains(&m))
    {
        return Some(Reason::Altitude);
    }
    match source.records.get(&report.address) {
        Some(previous) => {
            if let (Some(from), Some(to)) = (previous.position, report.position) {
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "microseconds between two reports"
                )]
                let seconds = report.heard_us.saturating_sub(previous.heard_us) as f64 / 1e6;
                if metres_between(from, to) > JITTER_M + MAX_SPEED_MPS * seconds {
                    return Some(Reason::Jump);
                }
            }
            None
        }
        None if source.records.len() >= MAX_PER_SOURCE => Some(Reason::TooMany),
        None => None,
    }
}

/// Great-circle distance, metres.
fn metres_between((lat1, lon1): (f64, f64), (lat2, lon2): (f64, f64)) -> f64 {
    const EARTH_M: f64 = 6_371_000.0;
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = p2 - p1;
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * EARTH_M * a.sqrt().asin()
}

fn aircraft(report: &TrafficReport, sources: Vec<String>, now_us: u64) -> TrafficAircraft {
    TrafficAircraft {
        address: format!("{:06X}", report.address & 0x00FF_FFFF),
        callsign: report.callsign.clone(),
        squawk: report.squawk.map(|code| format!("{code:04}")),
        category: category(report.category),
        protocol: match report.protocol {
            uxduck_core::domain::traffic::TrafficProtocol::Adsb => TrafficProtocol::Adsb,
            uxduck_core::domain::traffic::TrafficProtocol::Uat => TrafficProtocol::Uat,
        },
        latitude_deg: report.position.map(|(lat, _)| lat),
        longitude_deg: report.position.map(|(_, lon)| lon),
        altitude_m: report.altitude_m,
        altitude_frame: report.altitude_frame.map(|frame| match frame {
            AltitudeFrame::Pressure => TrafficAltitudeFrame::Pressure,
            AltitudeFrame::Geometric => TrafficAltitudeFrame::Geometric,
        }),
        track_deg: report.track_deg,
        ground_speed_mps: report.ground_speed_mps,
        climb_mps: report.climb_mps,
        simulated: report.simulated,
        heard_at_ms: report.heard_us / 1000,
        stale: now_us.saturating_sub(report.heard_us) >= STALE_US,
        sources,
        vehicle_id: None,
        own: None,
    }
}

fn category(category: TrafficCategory) -> CategoryDto {
    match category {
        TrafficCategory::Unknown => CategoryDto::Unknown,
        TrafficCategory::Light => CategoryDto::Light,
        TrafficCategory::Small => CategoryDto::Small,
        TrafficCategory::Large => CategoryDto::Large,
        TrafficCategory::Heavy => CategoryDto::Heavy,
        TrafficCategory::Fast => CategoryDto::Fast,
        TrafficCategory::Rotorcraft => CategoryDto::Rotorcraft,
        TrafficCategory::Glider => CategoryDto::Glider,
        TrafficCategory::Balloon => CategoryDto::Balloon,
        TrafficCategory::Parachute => CategoryDto::Parachute,
        TrafficCategory::Ultralight => CategoryDto::Ultralight,
        TrafficCategory::Uav => CategoryDto::Uav,
        TrafficCategory::Space => CategoryDto::Space,
        TrafficCategory::Surface => CategoryDto::Surface,
        TrafficCategory::Obstacle => CategoryDto::Obstacle,
    }
}

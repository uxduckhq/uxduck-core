//! Markers (ADR-0035): named things on the map besides vehicles, kept here
//! so every window, the agent and the CLI see the same ones. Reading needs
//! `read`; adding, changing and removing need `config`, as saved places do.
//!
//! A live marker (S179) follows a source: this computer's location, or a
//! GPS receiver the daemon reads through [`MarkerSourcePort`]. Its fixes
//! live in memory only (M11) and reach those who may see where people are
//! (`location`) as `marker/live`; its kept position is the one set by hand
//! or kept with "Keep it here".

use std::collections::BTreeMap;

use serde_json::{Value, json};
use uxduck_contracts::dto::link::IdParams;
use uxduck_contracts::dto::location::LocationState;
use uxduck_contracts::dto::marker::{
    Marker as MarkerDto, MarkerAddParams, MarkerFix, MarkerListResult, MarkerLive,
    MarkerLiveChanged, MarkerLiveState, MarkerUpdateParams,
};
use uxduck_contracts::dto::state::{ChangeOp, EntityChange, EntityType};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::{ClientRequest, ServerNotification};
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::marker::{Marker, MarkerLook, MarkerSource};
use uxduck_core::events::Change;

use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine, upsert};

const MAX_NAME: usize = 100;
/// A live marker's fixes go out at most this often; a change of state at once.
const LIVE_EVERY_MS: u64 = 500;
/// The speeds a GPS receiver is read at.
pub const BAUDS: [u32; 6] = [4800, 9600, 19_200, 38_400, 57_600, 115_200];

/// Reads the GPS receivers live markers follow, and tells the engine with
/// [`Engine::marker_fix`] and [`Engine::marker_source_failed`].
pub trait MarkerSourcePort: Send {
    /// The receivers to read now, by marker id; any no longer listed stop.
    fn watch(&mut self, sources: Vec<(String, MarkerSource)>);
}

#[derive(Default)]
pub(crate) struct Live {
    port: Option<Box<dyn MarkerSourcePort>>,
    latest: BTreeMap<String, MarkerLive>,
    sent_at_ms: BTreeMap<String, u64>,
    /// What the port was last told, so it is told only of changes.
    watching: Vec<(String, MarkerSource)>,
    /// The window sharing each device marker's position, and when it last did.
    sharers: BTreeMap<String, (ConnectionId, u64)>,
}

/// Another device may share a marker once its sharer has been quiet this long.
const SHARER_QUIET_MS: u64 = 10_000;

fn invalid(message: &str) -> ApiError {
    ApiError::new(ErrorKind::InvalidValue, message)
}

/// The name a new marker gets from its look (M7).
const fn default_name(look: MarkerLook) -> &'static str {
    match look {
        MarkerLook::Person => "Person",
        MarkerLook::GroundStation => "Ground station",
        MarkerLook::Boat => "Boat",
        MarkerLook::Car => "Car",
        MarkerLook::Flag => "Flag",
        MarkerLook::Pin => "Marker",
    }
}

fn name(text: &str) -> Result<String, ApiError> {
    let name = text.trim().to_owned();
    if name.is_empty() || name.chars().count() > MAX_NAME {
        return Err(invalid("a marker name must have 1–100 characters"));
    }
    Ok(name)
}

fn position(latitude_deg: f64, longitude_deg: f64) -> Result<(), ApiError> {
    if (-90.0..=90.0).contains(&latitude_deg) && (-180.0..=180.0).contains(&longitude_deg) {
        Ok(())
    } else {
        Err(invalid(
            "a marker needs a latitude within ±90° and a longitude within ±180°",
        ))
    }
}

/// A heading from 0 up to 360, with 360 itself as north.
fn source(source: MarkerSource) -> Result<MarkerSource, ApiError> {
    match &source {
        MarkerSource::Serial { port, baud } => {
            if port.trim().is_empty() || port.len() > 200 {
                return Err(invalid("choose the GPS receiver's serial port"));
            }
            if baud.is_some_and(|b| !BAUDS.contains(&b)) {
                return Err(invalid(
                    "a GPS receiver's speed is 4800, 9600, 19200, 38400, 57600 or 115200",
                ));
            }
        }
        MarkerSource::Udp { port: 0 } => {
            return Err(invalid("a UDP port is 1 to 65535"));
        }
        _ => {}
    }
    Ok(source)
}

fn heading(degrees: Option<f64>) -> Result<Option<f64>, ApiError> {
    match degrees {
        None => Ok(None),
        Some(d) if (0.0..=360.0).contains(&d) => Ok(Some(d % 360.0)),
        Some(_) => Err(invalid("a heading is 0° to 360°, clockwise from north")),
    }
}

/// Markers by name, then id, so lists read the same everywhere; with their
/// live fixes for a connection that may see where people are.
pub(crate) fn sorted(engine: &Engine, connection: ConnectionId) -> Vec<MarkerDto> {
    let see = engine.may_see_where(connection);
    let mut markers: Vec<&Marker> = engine.state.markers.values().collect();
    markers.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
    markers
        .into_iter()
        .map(|m| MarkerDto {
            live: see
                .then(|| engine.marker_live.latest.get(&m.id).cloned())
                .flatten(),
            ..m.into()
        })
        .collect()
}

#[allow(clippy::too_many_lines, reason = "one arm per method, each short")]
pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    let require = |capability: Capability| {
        if granted.contains(&capability) {
            Ok(())
        } else {
            Err(ApiError::new(
                ErrorKind::CapabilityDenied,
                capability.denied_message(),
            ))
        }
    };
    match request {
        ClientRequest::MarkerList(_) => {
            require(Capability::Read)?;
            to_value(&MarkerListResult {
                markers: sorted(engine, connection),
            })
        }
        ClientRequest::MarkerAdd(MarkerAddParams {
            name: given,
            look,
            latitude_deg,
            longitude_deg,
            heading_deg,
            source: from,
        }) => {
            require(Capability::Config)?;
            let look: MarkerLook = look.map(Into::into).unwrap_or_default();
            let name = name(given.as_deref().unwrap_or(default_name(look)))?;
            position(latitude_deg, longitude_deg)?;
            let marker = Marker {
                id: format!("mk{}", &engine.random.token()[..16]),
                name,
                look,
                latitude_deg,
                longitude_deg,
                heading_deg: heading(heading_deg)?,
                source: source(from.map(Into::into).unwrap_or_default())?,
                watch_links: true,
                created_at_us: engine.clock.now_us(),
                version: 1,
            };
            engine.commit(
                connection,
                Change::MarkerUpserted {
                    marker: marker.clone(),
                },
            );
            engine.watch_markers();
            to_value(&MarkerDto::from(&marker))
        }
        ClientRequest::MarkerUpdate(params) => {
            require(Capability::Config)?;
            let mut marker = engine
                .state
                .markers
                .get(&params.id)
                .ok_or_else(|| not_found(&params.id))?
                .clone();
            if marker.version != params.expected_version {
                let mut error = ApiError::new(
                    ErrorKind::VersionConflict,
                    "this marker changed in another window. Try again",
                );
                error.data.current_version = Some(marker.version);
                return Err(error);
            }
            // Set by hand again ("Keep it here"): where it was last seen,
            // unless a position is given.
            let kept = (marker.source != MarkerSource::Manual
                && matches!(
                    params.source,
                    Some(uxduck_contracts::dto::marker::MarkerSource::Manual)
                )
                && params.latitude_deg.is_none())
            .then(|| engine.marker_live.latest.get(&marker.id))
            .flatten()
            .and_then(|live| live.fix.clone());
            apply(&mut marker, params)?;
            if let Some(fix) = kept {
                marker.latitude_deg = fix.latitude_deg;
                marker.longitude_deg = fix.longitude_deg;
                if fix.heading_deg.is_some() {
                    marker.heading_deg = fix.heading_deg;
                }
            }
            marker.version += 1;
            engine.commit(
                connection,
                Change::MarkerUpserted {
                    marker: marker.clone(),
                },
            );
            engine.watch_markers();
            to_value(&MarkerDto::from(&marker))
        }
        ClientRequest::MarkerShare(params) => {
            require(Capability::Config)?;
            engine.share_marker(connection, &params)?;
            Ok(json!({}))
        }
        ClientRequest::MarkerUnshare(IdParams { id }) => {
            require(Capability::Config)?;
            engine.unshare_marker(connection, &id);
            Ok(json!({}))
        }
        ClientRequest::MarkerRemove(IdParams { id }) => {
            require(Capability::Config)?;
            if !engine.state.markers.contains_key(&id) {
                return Err(not_found(&id));
            }
            engine.commit(connection, Change::MarkerRemoved { id });
            engine.watch_markers();
            Ok(json!({}))
        }
        _ => Err(ApiError::new(ErrorKind::Internal, "not a marker method")),
    }
}

fn not_found(id: &str) -> ApiError {
    ApiError::new(ErrorKind::NotFound, format!("no marker {id}"))
}

fn apply(marker: &mut Marker, params: MarkerUpdateParams) -> Result<(), ApiError> {
    if let Some(given) = params.name {
        marker.name = name(&given)?;
    }
    if let Some(look) = params.look {
        marker.look = look.into();
    }
    if params.latitude_deg.is_some() != params.longitude_deg.is_some() {
        return Err(invalid(
            "move a marker with both a latitude and a longitude",
        ));
    }
    if let (Some(latitude_deg), Some(longitude_deg)) = (params.latitude_deg, params.longitude_deg) {
        position(latitude_deg, longitude_deg)?;
        marker.latitude_deg = latitude_deg;
        marker.longitude_deg = longitude_deg;
    }
    if let Some(degrees) = params.heading_deg {
        marker.heading_deg = heading(degrees)?;
    }
    if let Some(given) = params.source {
        marker.source = source(given.into())?;
    }
    if let Some(shown) = params.watch_links {
        marker.watch_links = shown;
    }
    Ok(())
}

impl Engine {
    /// Where the daemon reads GPS receivers; markers already following one
    /// start being read.
    pub fn set_marker_sources(&mut self, port: Box<dyn MarkerSourcePort>) {
        self.marker_live.port = Some(port);
        self.marker_live.watching.clear();
        self.watch_markers();
    }

    /// Brings live markers in line with the markers: the port reads the
    /// receivers they follow, the computer's follow its location, and those
    /// set by hand or gone are forgotten.
    pub(crate) fn watch_markers(&mut self) {
        let markers: Vec<(String, MarkerSource)> = self
            .state
            .markers
            .values()
            .map(|m| (m.id.clone(), m.source.clone()))
            .collect();
        let gone: Vec<String> = self
            .marker_live
            .latest
            .keys()
            .filter(|id| {
                !markers
                    .iter()
                    .any(|(m, s)| m == *id && *s != MarkerSource::Manual)
            })
            .cloned()
            .collect();
        for id in gone {
            self.marker_live.latest.remove(&id);
            self.marker_live.sent_at_ms.remove(&id);
            self.send_live(&id, None);
        }
        let read: Vec<(String, MarkerSource)> = markers
            .iter()
            .filter(|(_, s)| s.read_by_host())
            .cloned()
            .collect();
        if read != self.marker_live.watching {
            // A source changed: what it said before is not this one's.
            for (id, source) in &read {
                let was = self.marker_live.watching.iter().find(|(w, _)| w == id);
                if was.is_none_or(|(_, s)| s != source) {
                    self.marker_live.latest.remove(id);
                }
            }
            self.marker_live.watching.clone_from(&read);
            if let Some(port) = self.marker_live.port.as_mut() {
                port.watch(read.clone());
            }
        }
        // A device marker waits for a window to share; one that is no
        // longer one forgets its sharer.
        self.marker_live.sharers.retain(|id, _| {
            markers
                .iter()
                .any(|(m, s)| m == id && *s == MarkerSource::Device)
        });
        let devices: Vec<String> = markers
            .iter()
            .filter(|(_, s)| *s == MarkerSource::Device)
            .map(|(id, _)| id.clone())
            .collect();
        for id in devices {
            if !self.marker_live.latest.contains_key(&id) {
                self.set_live(
                    &id,
                    waiting("No device is sharing its position. Open this marker on the device and press Share this device's position."),
                    true,
                );
            }
        }
        for (id, _) in &read {
            if self.marker_live.latest.contains_key(id) {
                continue;
            }
            let live = if self.marker_live.port.is_some() {
                waiting("Waiting for a position from the GPS receiver.")
            } else {
                MarkerLive {
                    state: MarkerLiveState::Failed,
                    fix: None,
                    reason: Some("UXDuck Desktop reads GPS receivers. Open this there.".into()),
                    from: None,
                }
            };
            self.set_live(id, live, true);
        }
        self.computer_moved();
    }

    /// This computer's location changed (ADR-0030): markers that follow it move.
    pub(crate) fn computer_moved(&mut self) {
        let ids: Vec<String> = self
            .state
            .markers
            .values()
            .filter(|m| m.source == MarkerSource::Computer)
            .map(|m| m.id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        let status = self.location.status.clone();
        let live = match (status.state, status.fix) {
            (LocationState::On, Some(fix)) => MarkerLive {
                state: MarkerLiveState::Live,
                fix: Some(MarkerFix {
                    latitude_deg: fix.latitude_deg,
                    longitude_deg: fix.longitude_deg,
                    heading_deg: None,
                    accuracy_m: fix.accuracy_m,
                    satellites: None,
                    at_ms: fix.at_ms,
                }),
                reason: None,
                from: None,
            },
            (LocationState::Off, _) => waiting(
                "Location is off. Turn on Show your location in UXDuck Desktop to follow this computer.",
            ),
            (LocationState::Asking, _) => waiting("Waiting for this computer's location."),
            (_, fix) => MarkerLive {
                state: MarkerLiveState::Failed,
                fix: fix.map(|fix| MarkerFix {
                    latitude_deg: fix.latitude_deg,
                    longitude_deg: fix.longitude_deg,
                    heading_deg: None,
                    accuracy_m: fix.accuracy_m,
                    satellites: None,
                    at_ms: fix.at_ms,
                }),
                reason: Some(
                    status
                        .reason
                        .unwrap_or_else(|| "This computer's location isn't available.".into()),
                ),
                from: None,
            },
        };
        for id in ids {
            self.set_live(&id, live.clone(), true);
        }
    }

    fn share_marker(
        &mut self,
        connection: ConnectionId,
        params: &uxduck_contracts::dto::marker::MarkerShareParams,
    ) -> Result<(), ApiError> {
        let marker = self
            .state
            .markers
            .get(&params.id)
            .ok_or_else(|| not_found(&params.id))?;
        if marker.source != MarkerSource::Device {
            return Err(invalid(
                "this marker's position doesn't come from a device. Choose Position from › A phone or this device first",
            ));
        }
        position(params.latitude_deg, params.longitude_deg)?;
        let heading_deg = heading(params.heading_deg)?;
        let device = params.device.trim();
        if device.is_empty() || device.chars().count() > 60 {
            return Err(invalid("name the device sharing, in 1–60 characters"));
        }
        let now = self.clock.now_us() / 1000;
        if let Some((sharer, at)) = self.marker_live.sharers.get(&params.id)
            && *sharer != connection
            && self.connections.contains_key(sharer)
            && now.saturating_sub(*at) < SHARER_QUIET_MS
        {
            let from = self
                .marker_live
                .latest
                .get(&params.id)
                .and_then(|l| l.from.clone())
                .unwrap_or_else(|| "Another device".into());
            return Err(ApiError::new(
                ErrorKind::Busy,
                format!("{from} is sharing this marker's position. Stop it there first."),
            ));
        }
        self.marker_live
            .sharers
            .insert(params.id.clone(), (connection, now));
        let live = MarkerLive {
            state: MarkerLiveState::Live,
            fix: Some(MarkerFix {
                latitude_deg: params.latitude_deg,
                longitude_deg: params.longitude_deg,
                heading_deg,
                accuracy_m: params.accuracy_m.filter(|a| a.is_finite() && *a >= 0.0),
                satellites: None,
                at_ms: now,
            }),
            reason: None,
            from: Some(device.to_owned()),
        };
        self.set_live(&params.id, live, false);
        Ok(())
    }

    fn unshare_marker(&mut self, connection: ConnectionId, id: &str) {
        if self
            .marker_live
            .sharers
            .get(id)
            .is_some_and(|(sharer, _)| *sharer == connection)
        {
            self.stopped_sharing(id, "stopped sharing");
        }
    }

    /// Its sharer stopped or went: the last position stays, as last seen.
    fn stopped_sharing(&mut self, id: &str, how: &str) {
        self.marker_live.sharers.remove(id);
        let was = self.marker_live.latest.get(id).cloned();
        let from = was
            .as_ref()
            .and_then(|l| l.from.clone())
            .unwrap_or_else(|| "The device".into());
        let live = MarkerLive {
            state: MarkerLiveState::Failed,
            fix: was.and_then(|l| l.fix),
            reason: Some(format!("{from} {how}.")),
            from: Some(from),
        };
        self.set_live(id, live, true);
    }

    /// A window went: markers it shared stop.
    pub(crate) fn markers_disconnected(&mut self, connection: ConnectionId) {
        let shared: Vec<String> = self
            .marker_live
            .sharers
            .iter()
            .filter(|(_, (sharer, _))| *sharer == connection)
            .map(|(id, _)| id.clone())
            .collect();
        for id in shared {
            self.stopped_sharing(&id, "stopped sharing: it closed or lost its connection");
        }
    }

    /// A position from a marker's GPS receiver.
    pub fn marker_fix(&mut self, id: &str, fix: &crate::nmea::Fix) {
        if !self.marker_live.watching.iter().any(|(w, _)| w == id) {
            return;
        }
        let live = MarkerLive {
            state: MarkerLiveState::Live,
            fix: Some(MarkerFix {
                latitude_deg: fix.latitude_deg,
                longitude_deg: fix.longitude_deg,
                heading_deg: fix.heading_deg,
                accuracy_m: fix.accuracy_m,
                satellites: fix.satellites,
                at_ms: self.clock.now_us() / 1000,
            }),
            reason: None,
            from: None,
        };
        self.set_live(id, live, false);
    }

    /// A marker's GPS receiver cannot be read, or is no longer: why, in the
    /// app's words. The last position stays, as last seen.
    pub fn marker_source_failed(&mut self, id: &str, reason: String) {
        if !self.marker_live.watching.iter().any(|(w, _)| w == id) {
            return;
        }
        let fix = self
            .marker_live
            .latest
            .get(id)
            .and_then(|live| live.fix.clone());
        let live = MarkerLive {
            state: MarkerLiveState::Failed,
            fix,
            reason: Some(reason),
            from: None,
        };
        self.set_live(id, live, true);
    }

    fn set_live(&mut self, id: &str, live: MarkerLive, now: bool) {
        let changed_state = self
            .marker_live
            .latest
            .get(id)
            .is_none_or(|was| was.state != live.state || was.reason != live.reason);
        if self.marker_live.latest.get(id) == Some(&live) {
            return;
        }
        self.marker_live.latest.insert(id.to_owned(), live.clone());
        let at = self.clock.now_us() / 1000;
        let last = self.marker_live.sent_at_ms.get(id).copied();
        if now || changed_state || last.is_none_or(|t| at.saturating_sub(t) >= LIVE_EVERY_MS) {
            self.marker_live.sent_at_ms.insert(id.to_owned(), at);
            self.send_live(id, Some(live));
        }
    }

    fn send_live(&mut self, id: &str, live: Option<MarkerLive>) {
        let frame =
            crate::rpc::router::notification(&ServerNotification::MarkerLive(MarkerLiveChanged {
                id: id.to_owned(),
                live,
            }));
        let targets: Vec<ConnectionId> = self
            .subscribers
            .iter()
            .filter(|(_, types)| types.contains(&EntityType::Marker))
            .map(|(c, _)| *c)
            .filter(|c| self.may_see_where(*c))
            .collect();
        for connection in targets {
            self.send(connection, &frame);
        }
    }

    /// The markers watch links show (M9, S182), where each is shown now: a
    /// live one where its source last put it, else where it was set.
    #[must_use]
    pub fn edge_markers(&self) -> Vec<uxduck_contracts::dto::edge::RemoteMarker> {
        if !self.state.settings.watch_from_account {
            return Vec::new();
        }
        let mut markers: Vec<&Marker> = self
            .state
            .markers
            .values()
            .filter(|m| m.watch_links)
            .collect();
        markers.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        markers
            .into_iter()
            .map(|m| {
                let fix = (m.source != MarkerSource::Manual)
                    .then(|| self.marker_live.latest.get(&m.id))
                    .flatten()
                    .and_then(|live| live.fix.clone());
                uxduck_contracts::dto::edge::RemoteMarker {
                    id: m.id.clone(),
                    name: m.name.clone(),
                    look: m.look.into(),
                    latitude_deg: fix.as_ref().map_or(m.latitude_deg, |f| f.latitude_deg),
                    longitude_deg: fix.as_ref().map_or(m.longitude_deg, |f| f.longitude_deg),
                    heading_deg: fix.as_ref().and_then(|f| f.heading_deg).or(m.heading_deg),
                    fix_at_ms: fix.map(|f| f.at_ms),
                }
            })
            .collect()
    }

    /// Whether a connection may see where people are (ADR-0030 L4).
    pub(crate) fn may_see_where(&self, connection: ConnectionId) -> bool {
        self.connections
            .get(&connection)
            .and_then(|c| c.admitted.as_ref())
            .is_some_and(|a| {
                a.granted.contains(&Capability::Location) || a.extra.contains(&Capability::Location)
            })
    }
}

fn waiting(reason: &str) -> MarkerLive {
    MarkerLive {
        state: MarkerLiveState::Waiting,
        fix: None,
        reason: Some(reason.into()),
        from: None,
    }
}

/// The wire form of a marker change.
pub(crate) fn entity_changes(change: &Change) -> Vec<EntityChange> {
    match change {
        Change::MarkerUpserted { marker } => vec![EntityChange {
            marker: Some(marker.into()),
            ..upsert(EntityType::Marker, &marker.id)
        }],
        Change::MarkerRemoved { id } => vec![EntityChange {
            op: ChangeOp::Remove,
            ..upsert(EntityType::Marker, id)
        }],
        _ => Vec::new(),
    }
}

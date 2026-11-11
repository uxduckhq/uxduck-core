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
use uxduck_core::domain::marker::{Marker, MarkerLook, MarkerOutline, MarkerShape, MarkerSource};
use uxduck_core::domain::plan::LatLon;
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
        MarkerLook::Building => "Building",
    }
}

/// A new building's outline and height when none is given (ADR-0052 B3).
const NEW_BUILDING: MarkerOutline = MarkerOutline::Rectangle {
    width_m: 10.0,
    depth_m: 10.0,
};
const NEW_BUILDING_HEIGHT_M: f64 = 10.0;
const MAX_CORNERS: usize = 200;

fn outline(given: MarkerOutline) -> Result<MarkerOutline, ApiError> {
    let within = |m: f64, low: f64, high: f64| m.is_finite() && (low..=high).contains(&m);
    match &given {
        MarkerOutline::Rectangle { width_m, depth_m } => {
            if !within(*width_m, 0.5, 2000.0) || !within(*depth_m, 0.5, 2000.0) {
                return Err(invalid("a building's width and depth are 0.5 m to 2000 m"));
            }
        }
        MarkerOutline::Round { radius_m } => {
            if !within(*radius_m, 0.25, 1000.0) {
                return Err(invalid("a round building's radius is 0.25 m to 1000 m"));
            }
        }
        MarkerOutline::Corners { corners_m } => {
            if !(3..=MAX_CORNERS).contains(&corners_m.len()) {
                return Err(invalid("a building's outline has 3 to 200 corners"));
            }
            if corners_m.iter().any(|[east, north]| {
                !within(east.abs(), 0.0, 2000.0) || !within(north.abs(), 0.0, 2000.0)
            }) {
                return Err(invalid(
                    "a building's corners are within 2000 m of its point",
                ));
            }
            if shoelace(corners_m).abs() < 0.25 {
                return Err(invalid("a building's outline needs some area"));
            }
            if crosses(corners_m) {
                return Err(invalid("a building's outline must not cross itself"));
            }
        }
    }
    Ok(given)
}

/// Twice the signed area of a ring.
fn shoelace(ring: &[[f64; 2]]) -> f64 {
    (0..ring.len())
        .map(|i| {
            let ([ax, ay], [bx, by]) = (ring[i], ring[(i + 1) % ring.len()]);
            ax * by - bx * ay
        })
        .sum::<f64>()
        / 2.0
}

/// Whether two edges of a ring that share no corner cross.
fn crosses(ring: &[[f64; 2]]) -> bool {
    let count = ring.len();
    let side = |[ax, ay]: [f64; 2], [bx, by]: [f64; 2], [px, py]: [f64; 2]| {
        ((bx - ax) * (py - ay) - (by - ay) * (px - ax)).signum()
    };
    (0..count).any(|i| {
        let first = (ring[i], ring[(i + 1) % count]);
        ((i + 2)..count).any(|j| {
            if i == 0 && j == count - 1 {
                return false;
            }
            let second = (ring[j], ring[(j + 1) % count]);
            side(first.0, first.1, second.0) * side(first.0, first.1, second.1) < 0.0
                && side(second.0, second.1, first.0) * side(second.0, second.1, first.1) < 0.0
        })
    })
}

const MAX_POINTS: usize = 500;
/// A shape's points are all this close to its first (ADR-0053 D1).
const SHAPE_WITHIN_M: f64 = 100_000.0;
const M_PER_DEG: f64 = 111_320.0;

/// East and north of `origin`, in metres, on a plane touching it.
fn east_north(origin: LatLon, point: LatLon) -> [f64; 2] {
    let cos = origin.latitude_deg.to_radians().cos();
    [
        (point.longitude_deg - origin.longitude_deg) * M_PER_DEG * cos,
        (point.latitude_deg - origin.latitude_deg) * M_PER_DEG,
    ]
}

fn from_east_north(origin: LatLon, [east, north]: [f64; 2]) -> LatLon {
    let cos = origin.latitude_deg.to_radians().cos();
    LatLon {
        latitude_deg: origin.latitude_deg + north / M_PER_DEG,
        longitude_deg: origin.longitude_deg + east / (M_PER_DEG * cos),
    }
}

/// A line, an area or a circle as it is kept: a repeated closing corner
/// dropped, its points on the globe, few enough and close enough together.
fn shape(given: MarkerShape) -> Result<MarkerShape, ApiError> {
    let points = |mut points: Vec<LatLon>, fewest: usize, what: &str| {
        if points.len() > fewest && points.first() == points.last() {
            points.pop();
        }
        if !(fewest..=MAX_POINTS).contains(&points.len()) {
            return Err(invalid(&format!("{what} has {fewest} to 500 points")));
        }
        for point in &points {
            position(point.latitude_deg, point.longitude_deg)?;
        }
        let first = points[0];
        if points.iter().any(|p| {
            east_north(first, *p)
                .iter()
                .any(|m| m.abs() > SHAPE_WITHIN_M)
        }) {
            return Err(invalid(&format!(
                "{what}'s points are within 100 km of each other"
            )));
        }
        Ok(points)
    };
    match given {
        MarkerShape::Line { points: given } => {
            let points = points(given, 2, "a line")?;
            if length_m(&points) < 0.5 {
                return Err(invalid("a line needs two points apart"));
            }
            Ok(MarkerShape::Line { points })
        }
        MarkerShape::Area { points: given } => {
            let points = points(given, 3, "an area")?;
            let ring: Vec<[f64; 2]> = points.iter().map(|p| east_north(points[0], *p)).collect();
            if crosses(&ring) {
                return Err(invalid("an area's outline must not cross itself"));
            }
            if shoelace(&ring).abs() < 1.0 {
                return Err(invalid("an area needs some area"));
            }
            Ok(MarkerShape::Area { points })
        }
        MarkerShape::Circle { radius_m } => {
            if radius_m.is_finite() && (1.0..=50_000.0).contains(&radius_m) {
                Ok(MarkerShape::Circle { radius_m })
            } else {
                Err(invalid("a circle's radius is 1 m to 50 km"))
            }
        }
    }
}

fn length_m(points: &[LatLon]) -> f64 {
    points
        .windows(2)
        .map(|pair| {
            let [east, north] = east_north(pair[0], pair[1]);
            east.hypot(north)
        })
        .sum()
}

/// Where a line or an area's marker sits (D2): halfway along a line, at an
/// area's centre; a circle is about the marker's own point.
fn middle(shape: &MarkerShape) -> Option<LatLon> {
    match shape {
        MarkerShape::Line { points } => {
            let mut left = length_m(points) / 2.0;
            for pair in points.windows(2) {
                let [east, north] = east_north(pair[0], pair[1]);
                let leg = east.hypot(north);
                if leg >= left && leg > 0.0 {
                    let part = left / leg;
                    return Some(from_east_north(pair[0], [east * part, north * part]));
                }
                left -= leg;
            }
            points.last().copied()
        }
        MarkerShape::Area { points } => {
            let ring: Vec<[f64; 2]> = points.iter().map(|p| east_north(points[0], *p)).collect();
            let twice = 2.0 * shoelace(&ring);
            let (east, north) = (0..ring.len()).fold((0.0, 0.0), |(east, north), i| {
                let ([ax, ay], [bx, by]) = (ring[i], ring[(i + 1) % ring.len()]);
                let cross = ax * by - bx * ay;
                (east + (ax + bx) * cross, north + (ay + by) * cross)
            });
            Some(from_east_north(
                points[0],
                [east / (3.0 * twice), north / (3.0 * twice)],
            ))
        }
        MarkerShape::Circle { .. } => None,
    }
}

/// A line or an area moved whole from one point to another, keeping its
/// size and shape.
fn moved(shape: MarkerShape, from: LatLon, to: LatLon) -> MarkerShape {
    let shift = |points: Vec<LatLon>| {
        points
            .into_iter()
            .map(|p| from_east_north(to, east_north(from, p)))
            .collect()
    };
    match shape {
        MarkerShape::Line { points } => MarkerShape::Line {
            points: shift(points),
        },
        MarkerShape::Area { points } => MarkerShape::Area {
            points: shift(points),
        },
        circle @ MarkerShape::Circle { .. } => circle,
    }
}

const fn shape_word(shape: &MarkerShape) -> &'static str {
    match shape {
        MarkerShape::Line { .. } => "Line",
        MarkerShape::Area { .. } => "Area",
        MarkerShape::Circle { .. } => "Circle",
    }
}

fn height(metres: f64) -> Result<f64, ApiError> {
    if metres.is_finite() && (0.5..=1000.0).contains(&metres) {
        Ok(metres)
    } else {
        Err(invalid("a building's height is 0.5 m to 1000 m"))
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
            outline: footprint,
            height_m: tall,
            shape: drawn,
            colour,
            dashed,
            outline_only,
        }) => {
            require(Capability::Config)?;
            let inferred = if footprint.is_some() || tall.is_some() {
                MarkerLook::Building
            } else {
                MarkerLook::default()
            };
            let look: MarkerLook = look.map_or(inferred, Into::into);
            let building = look == MarkerLook::Building;
            if !building && (footprint.is_some() || tall.is_some()) {
                return Err(invalid("only a building has an outline and a height"));
            }
            let live = from
                .as_ref()
                .is_some_and(|s| *s != uxduck_contracts::dto::marker::MarkerSource::Manual);
            if building && live {
                return Err(invalid(
                    "a building is placed by hand and has no live source",
                ));
            }
            if building && drawn.is_some() {
                return Err(invalid("a building has an outline, not a line or an area"));
            }
            if live && drawn.is_some() {
                return Err(invalid(
                    "a line, an area or a circle is drawn by hand and has no live source",
                ));
            }
            let drawn = drawn.map(|s| shape(s.into())).transpose()?;
            let (latitude_deg, longitude_deg) =
                match (drawn.as_ref().and_then(middle), latitude_deg, longitude_deg) {
                    (Some(at), _, _) => (at.latitude_deg, at.longitude_deg),
                    (None, Some(latitude_deg), Some(longitude_deg)) => {
                        (latitude_deg, longitude_deg)
                    }
                    (None, _, _) => {
                        return Err(invalid("a marker needs a latitude and a longitude"));
                    }
                };
            let fallback = drawn.as_ref().map_or_else(
                || default_name(look).to_owned(),
                |s| {
                    let word = shape_word(s);
                    // One of these is free: there are fewer markers than numbers.
                    (1..=engine.state.markers.len() + 1)
                        .map(|n| format!("{word} {n}"))
                        .find(|name| !engine.state.markers.values().any(|m| m.name == *name))
                        .unwrap_or_else(|| word.to_owned())
                },
            );
            let name = name(given.as_deref().unwrap_or(&fallback))?;
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
                outline: building
                    .then(|| outline(footprint.map_or(NEW_BUILDING, Into::into)))
                    .transpose()?,
                height_m: building
                    .then(|| height(tall.unwrap_or(NEW_BUILDING_HEIGHT_M)))
                    .transpose()?,
                shape: drawn,
                colour: colour.map(Into::into),
                dashed: dashed.unwrap_or(false),
                outline_only: outline_only.unwrap_or(false),
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
    let building = marker.look == MarkerLook::Building;
    if let Some(look) = params.look {
        let look: MarkerLook = look.into();
        if building != (look == MarkerLook::Building) {
            return Err(invalid(
                "a building stays a building and a marker a marker. Add a new one instead",
            ));
        }
        marker.look = look;
    }
    if !building && (params.outline.is_some() || params.height_m.is_some()) {
        return Err(invalid("only a building has an outline and a height"));
    }
    if building
        && params
            .source
            .as_ref()
            .is_some_and(|s| *s != uxduck_contracts::dto::marker::MarkerSource::Manual)
    {
        return Err(invalid(
            "a building is placed by hand and has no live source",
        ));
    }
    if let Some(given) = params.outline {
        marker.outline = Some(outline(given.into())?);
    }
    if let Some(metres) = params.height_m {
        marker.height_m = Some(height(metres)?);
    }
    if params.latitude_deg.is_some() != params.longitude_deg.is_some() {
        return Err(invalid(
            "move a marker with both a latitude and a longitude",
        ));
    }
    let moving = params.latitude_deg.is_some();
    match params.shape {
        Some(Some(given)) => {
            if building {
                return Err(invalid("a building has an outline, not a line or an area"));
            }
            let given = shape(given.into())?;
            if let Some(at) = middle(&given) {
                if moving {
                    return Err(invalid(
                        "give a line or an area its points or a new position, not both",
                    ));
                }
                marker.latitude_deg = at.latitude_deg;
                marker.longitude_deg = at.longitude_deg;
            }
            marker.shape = Some(given);
        }
        Some(None) => marker.shape = None,
        None => {}
    }
    if let (Some(latitude_deg), Some(longitude_deg)) = (params.latitude_deg, params.longitude_deg) {
        position(latitude_deg, longitude_deg)?;
        let to = LatLon {
            latitude_deg,
            longitude_deg,
        };
        let from = LatLon {
            latitude_deg: marker.latitude_deg,
            longitude_deg: marker.longitude_deg,
        };
        marker.shape = marker.shape.take().map(|s| moved(s, from, to));
        marker.latitude_deg = latitude_deg;
        marker.longitude_deg = longitude_deg;
    }
    if let Some(colour) = params.colour {
        marker.colour = colour.map(Into::into);
    }
    if let Some(dashed) = params.dashed {
        marker.dashed = dashed;
    }
    if let Some(outline_only) = params.outline_only {
        marker.outline_only = outline_only;
    }
    if let Some(degrees) = params.heading_deg {
        marker.heading_deg = heading(degrees)?;
    }
    if let Some(given) = params.source {
        marker.source = source(given.into())?;
    }
    if marker.shape.is_some() && marker.source != MarkerSource::Manual {
        return Err(invalid(
            "a line, an area or a circle is drawn by hand and has no live source",
        ));
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
    /// Buildings wait until watchers can draw them (ADR-0052 B4).
    #[must_use]
    pub fn edge_markers(&self) -> Vec<uxduck_contracts::dto::edge::RemoteMarker> {
        if !self.watch_publishable() {
            return Vec::new();
        }
        let mut markers: Vec<&Marker> = self
            .state
            .markers
            .values()
            .filter(|m| m.watch_links && m.look != MarkerLook::Building && m.shape.is_none())
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

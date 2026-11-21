//! Global place search (S17/S22, ADR-0014 M6). The engine checks read access,
//! the operator's setting and inputs; the host performs a POST through the
//! existing UXDuck API. Queries and results are transient and trace-redacted.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_json::{Value, json};
use uxduck_contracts::dto::plan::LatLon;
use uxduck_contracts::dto::search::{
    ConfirmedPlaceArea, Coordinates, PlaceArea, PlaceAreaAnswerParams, PlaceAreaParams,
    PlaceAreaResult, PlaceAreasResult, PlaceLocateParams, PlaceLocateResult, PlaceSearchParams,
    PlaceSearchResult,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::{ClientRequest, ServerNotification};
use uxduck_core::domain::client::Capability;

use crate::{ConnectionId, Engine};

pub const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_PENDING: usize = 16;
const PER_CONNECTION: usize = 4;
const ATTRIBUTION: &str = "Powered by Esri";
/// The most of an outline from the UXDuck API that UXDuck reads (FB-177):
/// OpenStreetMap draws a place in as much detail as its mappers gave it,
/// which UXDuck does not control. Past it, the person is told and draws it.
pub const MAX_AREA_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const AREA_ATTRIBUTION: &str = "© OpenStreetMap contributors";
/// The corners of a confirmed outline its answer shows the assistant: the
/// agent reads at most 12,000 characters of a tool's answer
/// (`MAX_TOOL_OUTPUT_CHARS` in uxduck-agent) and a corner takes about 50, so
/// a whole outline, often hundreds of corners, would reach it cut off. The
/// plan takes the outline by its id instead, and the answer says so.
const PREVIEW_CORNERS: usize = 64;

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Search(PlaceSearchParams),
    Locate(PlaceLocateParams),
    /// A named area's outline, from OpenStreetMap (FB-177).
    Area(PlaceAreaParams),
}

impl Action {
    #[must_use]
    pub const fn path(&self) -> &'static str {
        match self {
            Self::Search(_) => "suggest",
            Self::Locate(_) => "locate",
            Self::Area(_) => "area",
        }
    }

    /// The most of an answer read for this lookup.
    #[must_use]
    pub const fn max_bytes(&self) -> usize {
        match self {
            Self::Area(_) => MAX_AREA_RESPONSE_BYTES,
            _ => MAX_RESPONSE_BYTES,
        }
    }

    /// An HTTP failure in words for this lookup: an outline not found, or not
    /// set up, says so and what to do instead.
    #[must_use]
    pub fn failed(&self, status: Option<u16>) -> ApiError {
        match (self, status) {
            (Self::Area(p), Some(404)) => ApiError::new(
                ErrorKind::NotFound,
                format!(
                    "No outline of \u{201c}{}\u{201d} was found in OpenStreetMap. Tell the person plainly, and ask them to draw the area on the map or to name the park, field or suburb it is in.",
                    p.name
                ),
            ),
            (Self::Area(p), Some(422)) => invalid(&format!(
                "OpenStreetMap's outline of \u{201c}{}\u{201d} is too detailed for UXDuck to send, and simplifying it would change its shape. Ask the person to draw the area on the map.",
                p.name
            )),
            (Self::Area(_), Some(503)) => invalid(
                "Outlines of named areas are not set up on the selected UXDuck API server. Ask the person to draw the area on the map.",
            ),
            _ => failed(status),
        }
    }

    /// An answer larger than [`Action::max_bytes`].
    #[must_use]
    pub fn too_large(&self) -> ApiError {
        match self {
            Self::Area(p) => invalid(&format!(
                "OpenStreetMap's outline of \u{201c}{}\u{201d} is more than {} MB, more detail than UXDuck reads. Ask the person to draw the area on the map.",
                p.name,
                MAX_AREA_RESPONSE_BYTES / (1024 * 1024)
            )),
            _ => failed(None),
        }
    }

    #[must_use]
    pub fn body(&self) -> Value {
        let mut body = match self {
            Self::Search(p) => json!({"q": p.query, "limit": p.limit.unwrap_or(5), "near": p.near}),
            Self::Locate(p) => json!({"key": p.key, "text": p.text, "near": p.near}),
            Self::Area(p) => json!({"q": p.name, "near": p.near}),
        };
        if body["near"].is_null()
            && let Some(map) = body.as_object_mut()
        {
            map.remove("near");
        }
        body
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Order {
    pub id: String,
    pub action: Action,
}

/// Completes every request with `Engine::place_lookup_done`, including failures.
pub trait PlacePort: Send {
    fn request(&mut self, order: Order);
}

#[derive(Default)]
pub(crate) struct Places {
    port: Option<Box<dyn PlacePort>>,
    pending: BTreeMap<String, (ConnectionId, Value)>,
    /// Outline lookups in `pending`, with the chat that asked.
    areas: BTreeMap<String, Option<String>>,
    /// Outlines on the map waiting for a person's word, by their id, each
    /// holding the `place/area` call that found it.
    waiting: BTreeMap<String, Waiting>,
    /// Connections that called `place/areas`.
    watchers: BTreeSet<ConnectionId>,
    /// Outlines a person confirmed, by id, for `plan/create`'s `placeAreas`.
    /// Kept until UXDuck restarts, like the lookups, which are never journaled.
    confirmed: BTreeMap<String, Confirmed>,
    /// The chat each agent connection's turn belongs to, learned from the
    /// host as the turn starts a message or a tool call, never from a
    /// request. Every turn, hosted or local, in the daemon or the browser,
    /// opens a connection of its own and closes it when it ends, so this
    /// names one chat, and a later turn of that chat can plan from an outline
    /// an earlier one found.
    turns: BTreeMap<ConnectionId, String>,
}

/// An outline a person confirmed, with the connection whose call found it
/// and the chat whose turn that was.
struct Confirmed {
    asker: ConnectionId,
    thread_id: Option<String>,
    area: PlaceArea,
}

struct Waiting {
    connection: ConnectionId,
    rpc_id: Value,
    area: PlaceArea,
}

fn invalid(message: &str) -> ApiError {
    ApiError::new(ErrorKind::InvalidValue, message)
}

fn near(spot: Option<Coordinates>) -> Result<Option<Coordinates>, ApiError> {
    spot.map(|p| {
        if !valid_position(p.latitude_deg, p.longitude_deg) {
            return Err(invalid(
                "The proximity hint needs a latitude within ±90° and longitude within ±180°.",
            ));
        }
        Ok(Coordinates {
            latitude_deg: (p.latitude_deg * 100.0).round() / 100.0,
            longitude_deg: (p.longitude_deg * 100.0).round() / 100.0,
        })
    })
    .transpose()
}

fn valid_position(lat: f64, lon: f64) -> bool {
    (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)
}

fn bounded(text: &str, max: usize) -> bool {
    !text.trim().is_empty() && text.chars().count() <= max
}

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
    operator: bool,
) -> Result<Value, ApiError> {
    if !granted.contains(&Capability::Read) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            Capability::Read.denied_message(),
        ));
    }
    let request = match request {
        ClientRequest::PlaceAreas(_) => {
            engine.places.watchers.insert(connection);
            return crate::rpc::handlers::to_value(&engine.waiting_areas(connection));
        }
        ClientRequest::PlaceAreaAnswer(params) if operator => {
            return engine.answer_area(connection, &params);
        }
        ClientRequest::PlaceAreaAnswer(_) => {
            return Err(ApiError::new(
                ErrorKind::CapabilityDenied,
                "Only a person says whether an outline on the map is the area.",
            ));
        }
        request => request,
    };
    engine.allow_place_search(connection)?;
    let action = match request {
        ClientRequest::PlaceSearch(mut p) => {
            p.query = p.query.trim().to_owned();
            if !(2..=200).contains(&p.query.chars().count())
                || !(1..=10).contains(&p.limit.unwrap_or(5))
            {
                return Err(invalid(
                    "Search with 2–200 characters and a limit from 1 to 10.",
                ));
            }
            p.near = near(p.near)?;
            Action::Search(p)
        }
        ClientRequest::PlaceLocate(mut p) => {
            if !bounded(&p.key, 1000) || !bounded(&p.text, 300) {
                return Err(invalid(
                    "Locate a suggestion using its key and full text from place/search.",
                ));
            }
            p.near = near(p.near)?;
            Action::Locate(p)
        }
        ClientRequest::PlaceArea(mut p) => {
            p.name = p.name.trim().to_owned();
            if !(2..=200).contains(&p.name.chars().count()) {
                return Err(invalid("Name the area with 2\u{2013}200 characters."));
            }
            p.near = near(p.near)?;
            Action::Area(p)
        }
        _ => return Err(ApiError::new(ErrorKind::Internal, "not a place lookup")),
    };
    if engine.places.port.is_none() {
        return Err(invalid(
            "Place search is unavailable on this host. Use saved places or coordinates.",
        ));
    }
    if engine.places.pending.len() >= MAX_PENDING
        || engine
            .places
            .pending
            .values()
            .filter(|(c, _)| *c == connection)
            .count()
            >= PER_CONNECTION
    {
        return Err(ApiError::new(
            ErrorKind::Busy,
            "Wait for the current place lookups to finish, then try again.",
        ));
    }
    let id = format!("ps{}", &engine.random.token()[..16]);
    if let Some(reply) = engine.replying_to.take() {
        engine.places.pending.insert(id.clone(), reply);
    }
    if let Action::Area(p) = &action {
        engine.places.areas.insert(id.clone(), p.thread_id.clone());
    }
    if let Some(port) = engine.places.port.as_mut() {
        port.request(Order { id, action });
    }
    Ok(Value::Null)
}

impl Engine {
    pub fn set_places(&mut self, port: Box<dyn PlacePort>) {
        self.places.port = Some(port);
    }

    fn allow_place_search(&self, connection: ConnectionId) -> Result<(), ApiError> {
        let operator = self
            .connections
            .get(&connection)
            .and_then(|c| c.operator_id.as_ref())
            .and_then(|id| self.state.operators.get(id));
        // MCP and one-shot CLI connections may have no operator selected.
        // An unattributed lookup cannot bypass an operator's opt-out.
        let allowed = operator.map_or_else(
            || {
                self.state
                    .operators
                    .values()
                    .all(|operator| operator.place_search)
            },
            |operator| operator.place_search,
        );
        if allowed {
            Ok(())
        } else {
            Err(invalid(
                "Place-name search is turned off for this operator. Use saved places or coordinates, or enable it in Settings.",
            ))
        }
    }

    pub fn place_lookup_done(&mut self, id: &str, result: Result<Value, ApiError>) {
        let asked_by = self.places.areas.remove(id);
        let Some((connection, rpc_id)) = self.places.pending.remove(id) else {
            return;
        };
        let result = self.allow_place_search(connection).and(result);
        // An outline found waits on the map for a person's word (FB-177): the
        // call is answered when they give it.
        let result = match (asked_by, result) {
            (Some(thread_id), Ok(found)) => match serde_json::from_value::<PlaceArea>(found) {
                Ok(area) => {
                    self.wait_for_answer(connection, rpc_id, area, thread_id);
                    return;
                }
                Err(_) => Err(failed(None)),
            },
            (_, result) => result,
        };
        let frame = match result {
            Ok(value) => json!({"jsonrpc": "2.0", "id": rpc_id, "result": value}),
            Err(error) => crate::rpc::router::error(&rpc_id, &error),
        };
        self.send(connection, &frame);
    }

    /// An agent connection's turn belongs to a chat, as its host says.
    pub(crate) fn note_turn(&mut self, connection: ConnectionId, thread_id: &str) {
        self.places.turns.insert(connection, thread_id.to_owned());
    }

    pub(crate) fn agent_thread(&self, connection: ConnectionId) -> Option<&str> {
        self.places.turns.get(&connection).map(String::as_str)
    }

    pub(crate) fn disconnect_places(&mut self, connection: ConnectionId) {
        self.places.turns.remove(&connection);
        self.places.pending.retain(|_, (c, _)| *c != connection);
        self.places
            .areas
            .retain(|id, _| self.places.pending.contains_key(id));
        self.places.watchers.remove(&connection);
        let before = self.places.waiting.len();
        self.places
            .waiting
            .retain(|_, w| w.connection != connection);
        if self.places.waiting.len() != before {
            self.send_areas();
        }
    }

    /// A `place/area` call stopped (its turn was stopped): a lookup still
    /// on its way is dropped when it lands, and an outline on the map leaves it.
    pub(crate) fn cancel_place_area(&mut self, connection: ConnectionId, request_id: &Value) {
        let stopped: Vec<String> = self
            .places
            .pending
            .iter()
            .filter(|(_, (c, rpc_id))| *c == connection && rpc_id == request_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in stopped {
            self.places.pending.remove(&id);
            self.places.areas.remove(&id);
        }
        let before = self.places.waiting.len();
        self.places
            .waiting
            .retain(|_, w| !(w.connection == connection && w.rpc_id == *request_id));
        if self.places.waiting.len() != before {
            self.send_areas();
        }
    }

    /// Whether a connection's person may see and answer an outline: one from
    /// a chat of their operator, or from a chat with none.
    fn shown_to(&self, connection: ConnectionId, area: &PlaceArea) -> bool {
        area.operator_id.as_ref().is_none_or(|operator| {
            self.connections
                .get(&connection)
                .and_then(|c| c.operator_id.as_ref())
                == Some(operator)
        })
    }

    fn waiting_areas(&self, connection: ConnectionId) -> PlaceAreasResult {
        PlaceAreasResult {
            areas: self
                .places
                .waiting
                .values()
                .filter(|w| self.shown_to(connection, &w.area))
                .map(|w| w.area.clone())
                .collect(),
        }
    }

    /// Each watcher gets the outlines its person may answer.
    fn send_areas(&mut self) {
        for id in self.places.watchers.clone() {
            self.send_areas_to(id);
        }
    }

    /// A watcher's outlines again, as when its operator changes.
    pub(crate) fn send_areas_to(&mut self, connection: ConnectionId) {
        if !self.places.watchers.contains(&connection) {
            return;
        }
        let frame = crate::rpc::router::notification(&ServerNotification::PlaceAreasChanged(
            self.waiting_areas(connection),
        ));
        self.send(connection, &frame);
    }

    /// An outline a person confirmed, for a plan to take exactly: only for
    /// the connection whose call found it, a later turn of the same chat, or
    /// a window of the operator whose chat asked. An outline from a chat with
    /// no operator stays with that chat's turns.
    pub(crate) fn confirmed_area(
        &self,
        id: &str,
        caller: ConnectionId,
    ) -> Result<&PlaceArea, ApiError> {
        let confirmed = self.places.confirmed.get(id).ok_or_else(|| {
            ApiError::new(
                ErrorKind::NotFound,
                format!("No confirmed outline {id}. Find the area's outline again and have the person confirm it on the map."),
            )
        })?;
        let operator = self
            .connections
            .get(&caller)
            .and_then(|c| c.operator_id.as_ref());
        if confirmed.asker == caller
            || (confirmed.thread_id.is_some()
                && confirmed.thread_id.as_ref() == self.places.turns.get(&caller))
            || (confirmed.area.operator_id.is_some()
                && confirmed.area.operator_id.as_ref() == operator)
        {
            return Ok(&confirmed.area);
        }
        Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            format!(
                "Outline {id} was confirmed for another chat. Find the area's outline from this one."
            ),
        ))
    }

    fn wait_for_answer(
        &mut self,
        connection: ConnectionId,
        rpc_id: Value,
        mut area: PlaceArea,
        thread_id: Option<String>,
    ) {
        area.id = format!("pa{}", &self.random.token()[..16]);
        area.operator_id = thread_id
            .as_ref()
            .and_then(|id| self.state.threads.get(id))
            .and_then(|thread| thread.operator_id.clone());
        area.thread_id = thread_id;
        self.places.waiting.insert(
            area.id.clone(),
            Waiting {
                connection,
                rpc_id,
                area,
            },
        );
        self.send_areas();
    }

    fn answer_area(
        &mut self,
        answerer: ConnectionId,
        answer: &PlaceAreaAnswerParams,
    ) -> Result<Value, ApiError> {
        let gone = || {
            ApiError::new(
                ErrorKind::NotFound,
                "That outline is no longer waiting. The chat that asked for it stopped.",
            )
        };
        let waiting = self.places.waiting.get(&answer.id).ok_or_else(gone)?;
        if !self.shown_to(answerer, &waiting.area) {
            return Err(ApiError::new(
                ErrorKind::CapabilityDenied,
                "Only the operator of the chat that asked for this outline can say whether it is the area.",
            ));
        }
        let Some(Waiting {
            connection,
            rpc_id,
            area,
        }) = self.places.waiting.remove(&answer.id)
        else {
            return Err(gone());
        };
        let frame = if answer.confirmed {
            let result = PlaceAreaResult {
                area: confirmed_answer(&area),
            };
            self.places.confirmed.insert(
                area.id.clone(),
                Confirmed {
                    asker: connection,
                    thread_id: self.places.turns.get(&connection).cloned(),
                    area,
                },
            );
            json!({"jsonrpc": "2.0", "id": rpc_id, "result": result})
        } else {
            crate::rpc::router::error(
                &rpc_id,
                &ApiError::new(
                    ErrorKind::NotFound,
                    format!(
                        "The person said OpenStreetMap's outline of \u{201c}{}\u{201d} is not the area. Ask them to draw it on the map, or which place they mean.",
                        area.name
                    ),
                ),
            )
        };
        self.send(connection, &frame);
        self.send_areas();
        Ok(json!({}))
    }
}

/// Cloudflare's per-address window at the UXDuck API, the shortest wait a 429 asks for.
const EDGE_PERIOD_S: u64 = 60;
/// The longest a UXDuck API limit lasts: the shared Esri allowance renews on the first of
/// each month, so a longer `Retry-After` is not believed.
const LONGEST_WAIT_S: u64 = 31 * 86_400;

/// A `Retry-After` in seconds, as the UXDuck API sends it with a 429.
#[must_use]
pub fn retry_after(value: &str) -> Option<u64> {
    value
        .trim()
        .parse()
        .ok()
        .filter(|s| (1..=LONGEST_WAIT_S).contains(s))
}

fn wait_words(seconds: u64) -> String {
    if seconds <= 60 {
        return "a minute".into();
    }
    let minutes = seconds.div_ceil(60);
    if minutes < 90 {
        return format!("{minutes} minutes");
    }
    let hours = seconds.div_ceil(3600);
    if hours < 36 {
        return format!("about {hours} hours");
    }
    format!("{} days", seconds.div_ceil(86_400))
}

/// The limit a 429 from the UXDuck API hit, as its `X-UXDuck-Limit` header names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Limit {
    /// A client address's minute at Cloudflare.
    Address,
    /// The shared Esri allowance, for a day or a month.
    Shared,
    /// OpenStreetMap's outlines, one request a second for all of UXDuck (FB-177).
    Outlines,
}

impl Limit {
    /// An `X-UXDuck-Limit` value, or none for a value this build does not know.
    #[must_use]
    pub fn from_header(value: &str) -> Option<Self> {
        match value.trim() {
            "address" => Some(Self::Address),
            "shared" => Some(Self::Shared),
            "outlines" => Some(Self::Outlines),
            _ => None,
        }
    }
}

/// A 429 from the UXDuck API, in the words the app shows, so the assistant reads the same.
/// An older API names no limit, and then a wait past a minute means the shared allowance.
#[must_use]
pub fn too_many(retry_after_s: Option<u64>, limit: Option<Limit>) -> ApiError {
    let limit = limit.unwrap_or(match retry_after_s {
        Some(seconds) if seconds > EDGE_PERIOD_S => Limit::Shared,
        _ => Limit::Address,
    });
    let message = match limit {
        Limit::Shared => format!(
            "UXDuck's shared place lookups from Esri are used up. Try again in {}, or enter coordinates.",
            wait_words(retry_after_s.unwrap_or(EDGE_PERIOD_S))
        ),
        Limit::Address => "Too many place lookups from this network in the last minute. Try again in a minute, or enter coordinates.".into(),
        Limit::Outlines => format!(
            "OpenStreetMap's outlines take one request a second for all of UXDuck, and the next turns are taken. Try again in {}, or draw the area on the map.",
            match retry_after_s.unwrap_or(1).min(EDGE_PERIOD_S) {
                1 => "a second".to_owned(),
                seconds => format!("{seconds} seconds"),
            }
        ),
    };
    ApiError::new(ErrorKind::Busy, message)
}

/// Map only the HTTP status; neither provider errors nor queried text enter logs.
#[must_use]
pub fn failed(status: Option<u16>) -> ApiError {
    match status {
        Some(429) => too_many(None, None),
        Some(404) => ApiError::new(
            ErrorKind::NotFound,
            "That place could not be located. Search again and choose another result.",
        ),
        Some(503) => invalid("Place search is not configured on the selected UXDuck API server."),
        _ => ApiError::new(
            ErrorKind::Internal,
            "The place-search service could not be reached or returned an invalid response. Try again or use coordinates.",
        ),
    }
}

/// The same checked reply for native HTTP and the browser worker's HTTP bridge.
///
/// # Errors
/// An unavailable, oversized or malformed provider response.
pub fn response(action: &Action, bytes: &[u8]) -> Result<Value, ApiError> {
    if bytes.len() > action.max_bytes() {
        return Err(action.too_large());
    }
    match action {
        Action::Search(p) => {
            let mut result: PlaceSearchResult =
                serde_json::from_slice(bytes).map_err(|_| failed(None))?;
            if result.suggestions.len() > p.limit.unwrap_or(5) as usize
                || result.suggestions.iter().any(|s| {
                    !bounded(&s.name, 300)
                        || s.detail.chars().count() > 1000
                        || !bounded(&s.key, 1000)
                        || !bounded(&s.text, 300)
                })
            {
                return Err(failed(None));
            }
            for suggestion in &mut result.suggestions {
                suggestion.near = p.near;
            }
            result.attribution = ATTRIBUTION.into();
            crate::rpc::handlers::to_value(&result)
        }
        Action::Locate(_) => {
            let mut result: PlaceLocateResult =
                serde_json::from_slice(bytes).map_err(|_| failed(None))?;
            if !bounded(&result.place.name, 300)
                || result.place.detail.chars().count() > 1000
                || !valid_position(result.place.latitude_deg, result.place.longitude_deg)
            {
                return Err(failed(None));
            }
            result.attribution = ATTRIBUTION.into();
            crate::rpc::handlers::to_value(&result)
        }
        Action::Area(_) => crate::rpc::handlers::to_value(&found_area(bytes)?),
    }
}

/// What the assistant reads of an outline the person confirmed: its id to
/// plan from, its bounds and a preview of its shape, never all of it.
fn confirmed_answer(area: &PlaceArea) -> ConfirmedPlaceArea {
    let points = &area.points;
    let (mut south, mut west, mut north, mut east) = (90.0_f64, 180.0_f64, -90.0_f64, -180.0_f64);
    for p in points {
        south = south.min(p.latitude_deg);
        north = north.max(p.latitude_deg);
        west = west.min(p.longitude_deg);
        east = east.max(p.longitude_deg);
    }
    let corners = points.len();
    let shown = corners.min(PREVIEW_CORNERS);
    let preview = (0..shown).map(|i| points[i * corners / shown]).collect();
    let shape = if shown == corners {
        format!("The preview shows all {corners} corners.")
    } else {
        format!(
            "The preview shows {shown} of its {corners} corners, evenly spaced, only to see its shape."
        )
    };
    ConfirmedPlaceArea {
        id: area.id.clone(),
        name: area.name.clone(),
        detail: area.detail.clone(),
        kind: area.kind.clone(),
        corners: u32::try_from(corners).unwrap_or(u32::MAX),
        south_deg: south,
        west_deg: west,
        north_deg: north,
        east_deg: east,
        preview,
        how_to_plan: format!(
            "The person confirmed this outline. To survey it, give plan_create placeAreas with the survey pattern's id and \u{201c}{}\u{201d}, and the survey's area as []. The plan then takes exactly the {corners} confirmed corners. {shape} Never plan from the preview.",
            area.id
        ),
        note: area.note.clone(),
        attribution: area.attribution.clone(),
    }
}

/// The outline the UXDuck API found, checked, with what it leaves out of
/// OpenStreetMap's said in words, never dropped silently.
fn found_area(bytes: &[u8]) -> Result<PlaceArea, ApiError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Found {
        area: FoundArea,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct FoundArea {
        name: String,
        detail: String,
        kind: String,
        points: Vec<LatLon>,
        /// The parts OpenStreetMap draws the place in; the largest is `points`.
        parts: u32,
        /// Holes inside `points`, such as a pond.
        holes: u32,
        /// How far from OpenStreetMap's outline `points` may run, once
        /// simplified to fit one answer.
        #[serde(default)]
        tolerance_m: Option<f64>,
        /// How many corners OpenStreetMap's outline had, when the API
        /// simplified it to fewer.
        #[serde(default)]
        corners_found: Option<u32>,
    }
    let Found { area } = serde_json::from_slice(bytes).map_err(|_| failed(None))?;
    if !bounded(&area.name, 300)
        || area.detail.chars().count() > 1000
        || area.kind.chars().count() > 100
        || area.points.len() < 3
        || area.parts == 0
        || area
            .points
            .iter()
            .any(|p| !valid_position(p.latitude_deg, p.longitude_deg))
    {
        return Err(failed(None));
    }
    let mut left_out = Vec::new();
    let within = area
        .tolerance_m
        .filter(|t| t.is_finite() && *t > 0.0)
        .map(|t| format!("{:.0}", t.max(1.0)));
    match (area.corners_found, within) {
        (Some(found), Some(within)) if found as usize > area.points.len() => left_out.push(format!(
            "OpenStreetMap's outline has {found} corners. It was simplified to {}, within about {within} m of it, to fit the most one answer brings.",
            area.points.len()
        )),
        (_, Some(within)) => left_out.push(format!(
            "It follows OpenStreetMap's outline to within about {within} m."
        )),
        _ => {}
    }
    if area.parts > 1 {
        let others = area.parts - 1;
        left_out.push(format!(
            "OpenStreetMap draws it in {} parts. This outline is the largest, and the other {} {} left out.",
            area.parts,
            others,
            if others == 1 { "is" } else { "are" }
        ));
    }
    if area.holes > 0 {
        left_out.push(format!(
            "It has {} {} inside, such as a pond or a building, which this outline covers.",
            area.holes,
            if area.holes == 1 { "hole" } else { "holes" }
        ));
    }
    Ok(PlaceArea {
        id: String::new(),
        name: area.name,
        detail: area.detail,
        kind: area.kind,
        points: area.points,
        note: (!left_out.is_empty()).then(|| left_out.join(" ")),
        attribution: AREA_ATTRIBUTION.into(),
        thread_id: None,
        operator_id: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_and_malformed_responses_do_not_look_like_empty_searches() {
        let search = Action::Search(PlaceSearchParams {
            query: "Queenstown".into(),
            limit: None,
            near: None,
        });
        assert!(
            response(
                &search,
                br#"{"suggestions":[],"attribution":"Powered by Esri"}"#
            )
            .is_ok()
        );
        for bytes in [b"{}".as_slice(), b"null".as_slice(), b"not JSON".as_slice()] {
            assert!(response(&search, bytes).is_err());
        }
        assert_eq!(failed(Some(429)).kind(), ErrorKind::Busy);
        assert_eq!(failed(Some(429)).message, too_many(None, None).message);
        assert_eq!(failed(Some(404)).kind(), ErrorKind::NotFound);
        assert_eq!(search.body(), json!({"q":"Queenstown","limit":5}));
        let locate = Action::Locate(PlaceLocateParams {
            key: "a".into(),
            text: "Queenstown".into(),
            near: None,
        });
        assert!(response(&locate, br#"{"place":{"name":"Nowhere","detail":"","latitudeDeg":91,"longitudeDeg":0},"attribution":"Powered by Esri"}"#).is_err());
    }

    #[test]
    fn a_limit_says_when_to_try_again() {
        let words = |retry: &str| too_many(retry_after(retry), None).message;
        assert!(words("60").contains("in a minute"));
        assert!(words("61").contains("in 2 minutes"));
        assert!(words("18000").contains("in about 5 hours"));
        assert!(words("1166400").contains("in 14 days"));
        for unbelievable in ["0", "-5", "soon", "2678401"] {
            assert_eq!(retry_after(unbelievable), None, "{unbelievable}");
        }
        assert_eq!(too_many(None, None).kind(), ErrorKind::Busy);
        assert_eq!(
            too_many(Some(30), Limit::from_header("shared")).message,
            "UXDuck's shared place lookups from Esri are used up. Try again in a minute, or enter coordinates."
        );
        assert_eq!(
            too_many(Some(464), Limit::from_header("address")).message,
            "Too many place lookups from this network in the last minute. Try again in a minute, or enter coordinates."
        );
        assert_eq!(Limit::from_header("elsewhere"), None);
        assert_eq!(
            too_many(Some(4), Limit::from_header("outlines")).message,
            "OpenStreetMap's outlines take one request a second for all of UXDuck, and the next turns are taken. Try again in 4 seconds, or draw the area on the map."
        );
        assert!(
            too_many(Some(1), Limit::from_header("outlines"))
                .message
                .contains("Try again in a second, or draw")
        );
        let area = Action::Area(PlaceAreaParams {
            name: "Central Park".into(),
            near: None,
            thread_id: None,
        });
        assert!(
            area.failed(Some(422))
                .message
                .contains("too detailed for UXDuck to send")
        );
    }
}

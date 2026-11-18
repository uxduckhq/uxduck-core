//! Global place search (S17/S22, ADR-0014 M6). The engine checks read access,
//! the operator's setting and inputs; the host performs a POST through the
//! existing UXDuck API. Queries and results are transient and trace-redacted.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use uxduck_contracts::dto::search::{
    Coordinates, PlaceLocateParams, PlaceLocateResult, PlaceSearchParams, PlaceSearchResult,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;

use crate::{ConnectionId, Engine};

pub const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_PENDING: usize = 16;
const PER_CONNECTION: usize = 4;
const ATTRIBUTION: &str = "Powered by Esri";

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Search(PlaceSearchParams),
    Locate(PlaceLocateParams),
}

impl Action {
    #[must_use]
    pub const fn path(&self) -> &'static str {
        match self {
            Self::Search(_) => "suggest",
            Self::Locate(_) => "locate",
        }
    }

    #[must_use]
    pub fn body(&self) -> Value {
        let mut body = match self {
            Self::Search(p) => json!({"q": p.query, "limit": p.limit.unwrap_or(5), "near": p.near}),
            Self::Locate(p) => json!({"key": p.key, "text": p.text, "near": p.near}),
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
) -> Result<Value, ApiError> {
    if !granted.contains(&Capability::Read) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            Capability::Read.denied_message(),
        ));
    }
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
        let Some((connection, rpc_id)) = self.places.pending.remove(id) else {
            return;
        };
        let result = self.allow_place_search(connection).and(result);
        let frame = match result {
            Ok(value) => json!({"jsonrpc": "2.0", "id": rpc_id, "result": value}),
            Err(error) => crate::rpc::router::error(&rpc_id, &error),
        };
        self.send(connection, &frame);
    }

    pub(crate) fn disconnect_places(&mut self, connection: ConnectionId) {
        self.places.pending.retain(|_, (c, _)| *c != connection);
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
}

impl Limit {
    /// An `X-UXDuck-Limit` value, or none for a value this build does not know.
    #[must_use]
    pub fn from_header(value: &str) -> Option<Self> {
        match value.trim() {
            "address" => Some(Self::Address),
            "shared" => Some(Self::Shared),
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
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(failed(None));
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
    }
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
    }
}

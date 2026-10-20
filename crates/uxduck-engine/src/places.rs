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

/// Map only the HTTP status; neither provider errors nor queried text enter logs.
#[must_use]
pub fn failed(status: Option<u16>) -> ApiError {
    match status {
        Some(429) => ApiError::new(
            ErrorKind::Busy,
            "Too many place searches. Try again in a minute.",
        ),
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
        assert_eq!(failed(Some(404)).kind(), ErrorKind::NotFound);
        assert_eq!(search.body(), json!({"q":"Queenstown","limit":5}));
        let locate = Action::Locate(PlaceLocateParams {
            key: "a".into(),
            text: "Queenstown".into(),
            near: None,
        });
        assert!(response(&locate, br#"{"place":{"name":"Nowhere","detail":"","latitudeDeg":91,"longitudeDeg":0},"attribution":"Powered by Esri"}"#).is_err());
    }
}

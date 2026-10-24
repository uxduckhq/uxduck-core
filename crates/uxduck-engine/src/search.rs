//! Search from the input (ADR-0009): one method over what the daemon knows,
//! shared by the UI, the agent and the MCP server. Matching is plain and
//! local: a title that starts with the query ranks first, then one with a
//! word that does, then one that contains it anywhere. No network, and
//! nothing about the query is kept.

use serde_json::{Value, json};
use uxduck_contracts::dto::link::IdParams;
use uxduck_contracts::dto::search::{
    Coordinates, PlaceSaveParams, PlaceUpdateParams, SavedPlace as PlaceDto, SearchHit, SearchKind,
    SearchQueryParams, SearchResult,
};
use uxduck_contracts::dto::state::{EntityChange, EntityType};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::place::SavedPlace;
use uxduck_core::events::Change;

use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine, upsert};

/// The sections of Settings and the other pages a search can open.
const SETTINGS: [(&str, &str); 13] = [
    ("account", "Account and product updates"),
    ("units", "Units"),
    ("coordinates", "Coordinate format"),
    ("map-engine", "Map engine"),
    ("data", "Data and analytics"),
    ("place-search", "Place-name search"),
    ("agents", "Agents and tools"),
    ("daemon", "When the last window closes"),
    ("storage", "Storage"),
    ("keyboard", "Keyboard shortcuts"),
    ("confirmations", "Confirmations and approvals"),
    ("parameters", "Parameter descriptions"),
    ("plugins", "Plugins"),
];

/// How well `text` matches `query` (both lower case): 3 starts with it, 2
/// a word starts with it, 1 contains it, 0 not at all.
fn score(text: &str, query: &str) -> u8 {
    let text = text.to_lowercase();
    if text.starts_with(query) {
        3
    } else if text
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| word.starts_with(query))
    {
        2
    } else {
        u8::from(text.contains(query))
    }
}

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
        ClientRequest::SearchQuery(params) => {
            require(Capability::Read)?;
            to_value(&search(engine, connection, &params))
        }
        ClientRequest::PlaceSave(PlaceSaveParams {
            name,
            latitude_deg,
            longitude_deg,
        }) => {
            require(Capability::Config)?;
            let name = name.trim().to_owned();
            let valid = !name.is_empty()
                && name.chars().count() <= 100
                && (-90.0..=90.0).contains(&latitude_deg)
                && (-180.0..=180.0).contains(&longitude_deg);
            if !valid {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "a place needs a name and a latitude and longitude on the globe",
                ));
            }
            let place = SavedPlace {
                id: format!("pl{}", &engine.random.token()[..16]),
                operator_id: engine.actor(connection).operator_id,
                name,
                latitude_deg,
                longitude_deg,
                created_at_us: engine.clock.now_us(),
                version: 1,
            };
            engine.commit(
                connection,
                Change::PlaceUpserted {
                    place: place.clone(),
                },
            );
            to_value(&PlaceDto::from(&place))
        }
        ClientRequest::PlaceUpdate(PlaceUpdateParams {
            id,
            expected_version,
            name,
        }) => {
            require(Capability::Config)?;
            let mut place = editable_place(engine, connection, &id)?.clone();
            if place.version != expected_version {
                let mut error = ApiError::new(
                    ErrorKind::VersionConflict,
                    "this place changed in another window. Cancel and rename it again",
                );
                error.data.current_version = Some(place.version);
                return Err(error);
            }
            let name = name.trim().to_owned();
            if name.is_empty() || name.chars().count() > 100 {
                return Err(ApiError::new(
                    ErrorKind::InvalidValue,
                    "a place name must have 1–100 characters",
                ));
            }
            place.name = name;
            place.version += 1;
            engine.commit(
                connection,
                Change::PlaceUpserted {
                    place: place.clone(),
                },
            );
            to_value(&PlaceDto::from(&place))
        }
        ClientRequest::PlaceRemove(IdParams { id }) => {
            require(Capability::Config)?;
            editable_place(engine, connection, &id)?;
            engine.commit(connection, Change::PlaceRemoved { id });
            Ok(json!({}))
        }
        _ => Err(ApiError::new(ErrorKind::Internal, "not a search method")),
    }
}

fn editable_place<'a>(
    engine: &'a Engine,
    connection: ConnectionId,
    id: &str,
) -> Result<&'a SavedPlace, ApiError> {
    let place = engine
        .state
        .places
        .get(id)
        .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no place {id}")))?;
    if place.operator_id.is_some() && place.operator_id != engine.actor(connection).operator_id {
        return Err(ApiError::new(ErrorKind::NotFound, format!("no place {id}")));
    }
    Ok(place)
}

struct Hit {
    score: u8,
    hit: SearchHit,
}

fn hit(kind: SearchKind, id: String, title: String, subtitle: String) -> SearchHit {
    SearchHit {
        kind,
        id,
        title,
        subtitle,
        vehicle_id: None,
        place: None,
    }
}

fn search(engine: &Engine, connection: ConnectionId, params: &SearchQueryParams) -> SearchResult {
    let query = params.query.trim().to_lowercase();
    if query.is_empty() {
        return SearchResult::default();
    }
    let wanted = |kind: SearchKind| params.kinds.as_ref().is_none_or(|k| k.contains(&kind));
    let limit = usize::try_from(params.limit.unwrap_or(20).clamp(1, 100)).unwrap_or(20);
    let mut hits: Vec<Hit> = Vec::new();
    let mut add = |score: u8, hit: SearchHit| {
        if score > 0 {
            hits.push(Hit { score, hit });
        }
    };
    for collect in [
        vehicles_and_chats,
        parameters_and_missions,
        telemetry,
        settings_and_places,
        notes,
    ] {
        collect(engine, connection, &query, &wanted, &mut add);
    }
    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.hit.title.len().cmp(&b.hit.title.len()))
            .then_with(|| a.hit.title.cmp(&b.hit.title))
    });
    SearchResult {
        hits: hits.into_iter().take(limit).map(|h| h.hit).collect(),
    }
}

/// Notes in vehicles' history, by their words (S69); deleted ones are not
/// found.
fn notes(
    engine: &Engine,
    _connection: ConnectionId,
    query: &str,
    wanted: &dyn Fn(SearchKind) -> bool,
    add: &mut dyn FnMut(u8, SearchHit),
) {
    if !wanted(SearchKind::Note) {
        return;
    }
    for entry in engine.state.history.values() {
        let uxduck_core::domain::history::EntryKind::Note { text, .. } = &entry.kind else {
            continue;
        };
        if entry.deleted.is_some() {
            continue;
        }
        let vehicle = engine
            .state
            .vehicles
            .values()
            .find(|v| v.id == entry.vehicle_id || v.aliases.contains(&entry.vehicle_id));
        let title: String = text.chars().take(80).collect();
        add(
            score(text, query),
            SearchHit {
                vehicle_id: vehicle.map(|v| v.id.clone()),
                ..hit(
                    SearchKind::Note,
                    entry.id.clone(),
                    title,
                    format!(
                        "History · {}",
                        vehicle.map_or("a vehicle", |v| v.name.as_str())
                    ),
                )
            },
        );
    }
}

fn vehicles_and_chats(
    engine: &Engine,
    connection: ConnectionId,
    query: &str,
    wanted: &dyn Fn(SearchKind) -> bool,
    add: &mut dyn FnMut(u8, SearchHit),
) {
    if wanted(SearchKind::Vehicle) {
        for vehicle in engine.state.vehicles.values().filter(|v| !v.dismissed) {
            let own = score(&vehicle.name, query).max(score(&vehicle.vehicle_type, query) / 2);
            // By a label too (S106), below a name that starts so, saying which.
            let label = vehicle
                .labels
                .iter()
                .map(|l| (score(l, query).min(2), l))
                .max_by_key(|(s, _)| *s)
                .filter(|(s, _)| *s > own);
            let (s, section) = label.map_or_else(
                || (own, "Vehicles".to_owned()),
                |(s, l)| (s, format!("Vehicles · {l}")),
            );
            add(
                s,
                SearchHit {
                    vehicle_id: Some(vehicle.id.clone()),
                    ..hit(
                        SearchKind::Vehicle,
                        vehicle.id.clone(),
                        vehicle.name.clone(),
                        section,
                    )
                },
            );
        }
    }
    if wanted(SearchKind::Chat) {
        for thread in engine.state.threads.values().filter(|t| {
            t.deleted_at_us.is_none()
                && crate::rpc::threads::sees(engine, connection, t.operator_id.as_deref())
        }) {
            let folder = thread
                .folder_id
                .as_ref()
                .and_then(|id| engine.state.folders.get(id))
                .map_or_else(|| "Chats".to_owned(), |f| format!("Chats · {}", f.name));
            add(
                score(&thread.title, query),
                SearchHit {
                    vehicle_id: thread.vehicle_id.clone(),
                    ..hit(
                        SearchKind::Chat,
                        thread.id.clone(),
                        thread.title.clone(),
                        folder,
                    )
                },
            );
        }
    }
    if wanted(SearchKind::Folder) {
        for folder in engine.state.folders.values().filter(|f| {
            f.deleted_at_us.is_none()
                && crate::rpc::threads::sees(engine, connection, f.operator_id.as_deref())
        }) {
            add(
                score(&folder.name, query),
                hit(
                    SearchKind::Folder,
                    folder.id.clone(),
                    folder.name.clone(),
                    "Chats".into(),
                ),
            );
        }
    }
}

fn parameters_and_missions(
    engine: &Engine,
    _connection: ConnectionId,
    query: &str,
    wanted: &dyn Fn(SearchKind) -> bool,
    add: &mut dyn FnMut(u8, SearchHit),
) {
    let vehicle_name = |id: &str| {
        engine
            .vehicle(id)
            .map_or_else(|| "a vehicle".to_owned(), |v| v.name.clone())
    };
    if wanted(SearchKind::Parameter) {
        // What each vehicle's parameters mean, where its set is here (S67).
        let mut sets = std::collections::BTreeMap::new();
        for parameter in engine.state.parameters.values() {
            let set = sets
                .entry(parameter.vehicle_id.as_str())
                .or_insert_with(|| {
                    let helicopter = engine
                        .vehicle(&parameter.vehicle_id)
                        .is_some_and(|v| v.vehicle_type == "Helicopter");
                    (
                        engine.vehicle_definitions(&parameter.vehicle_id),
                        helicopter,
                    )
                });
            let definition = set
                .0
                .as_deref()
                .and_then(|s| s.find(&parameter.name, set.1));
            let meaning = definition.map_or(0, |d| d.meaning_score(query));
            let what = definition.map_or_else(String::new, |d| format!(" · {}", d.display_name));
            add(
                score(&parameter.name, query).max(meaning),
                SearchHit {
                    vehicle_id: Some(parameter.vehicle_id.clone()),
                    ..hit(
                        SearchKind::Parameter,
                        parameter.id(),
                        parameter.name.clone(),
                        format!(
                            "Parameters · {} = {}{what}",
                            vehicle_name(&parameter.vehicle_id),
                            parameter.value
                        ),
                    )
                },
            );
        }
    }
    if wanted(SearchKind::Mission) {
        for mission in engine.state.missions.values() {
            let name = vehicle_name(&mission.vehicle_id);
            let title = format!("Mission of {name}");
            add(
                score(&title, query),
                SearchHit {
                    vehicle_id: Some(mission.vehicle_id.clone()),
                    ..hit(
                        SearchKind::Mission,
                        mission.vehicle_id.clone(),
                        title,
                        format!("{} items", mission.items.len().saturating_sub(1)),
                    )
                },
            );
        }
    }
}

fn telemetry(
    engine: &Engine,
    _connection: ConnectionId,
    query: &str,
    wanted: &dyn Fn(SearchKind) -> bool,
    add: &mut dyn FnMut(u8, SearchHit),
) {
    let vehicle_name = |id: &str| {
        engine
            .vehicle(id)
            .map_or_else(|| "a vehicle".to_owned(), |v| v.name.clone())
    };
    if wanted(SearchKind::Telemetry) {
        for telemetry in engine.telemetry.values() {
            let name = vehicle_name(&telemetry.vehicle_id);
            for item in &telemetry.items {
                add(
                    score(&item.label, query).max(score(&item.key, query)),
                    SearchHit {
                        vehicle_id: Some(telemetry.vehicle_id.clone()),
                        ..hit(
                            SearchKind::Telemetry,
                            format!("{}|{}", telemetry.vehicle_id, item.key),
                            item.label.clone(),
                            format!("Telemetry · {name}"),
                        )
                    },
                );
            }
        }
    }
}

fn settings_and_places(
    engine: &Engine,
    connection: ConnectionId,
    query: &str,
    wanted: &dyn Fn(SearchKind) -> bool,
    add: &mut dyn FnMut(u8, SearchHit),
) {
    if wanted(SearchKind::Setting) {
        for (id, title) in SETTINGS {
            add(
                score(title, query),
                hit(
                    SearchKind::Setting,
                    id.into(),
                    title.into(),
                    "Settings".into(),
                ),
            );
        }
    }
    if wanted(SearchKind::Place) {
        let operator = engine.actor(connection).operator_id;
        for place in engine
            .state
            .places
            .values()
            .filter(|p| p.operator_id.is_none() || p.operator_id == operator)
        {
            add(
                score(&place.name, query),
                SearchHit {
                    place: Some(Coordinates {
                        latitude_deg: place.latitude_deg,
                        longitude_deg: place.longitude_deg,
                    }),
                    ..hit(
                        SearchKind::Place,
                        place.id.clone(),
                        place.name.clone(),
                        "Saved places".into(),
                    )
                },
            );
        }
    }
}

/// The wire form of a place change.
pub(crate) fn entity_changes(change: &Change) -> Vec<EntityChange> {
    match change {
        Change::PlaceUpserted { place } => vec![EntityChange {
            place: Some(place.into()),
            ..upsert(EntityType::Place, &place.id)
        }],
        Change::PlaceRemoved { id } => vec![EntityChange {
            op: uxduck_contracts::dto::state::ChangeOp::Remove,
            ..upsert(EntityType::Place, id)
        }],
        _ => Vec::new(),
    }
}

//! Operations (ADR-0039 R16, S213): flights of two or more vehicles that
//! overlap or follow within 30 minutes are one operation, made by itself;
//! a person can make one, name it and gather flights into it. Grouping
//! leaves a person's operations alone and never files a single vehicle's
//! flight anywhere.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use uxduck_contracts::dto::Empty;
use uxduck_contracts::dto::operation::{
    Operation as OperationDto, OperationCreateParams, OperationFlightsParams, OperationListResult,
    OperationRenameParams,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::rpc::ClientRequest;
use uxduck_core::domain::client::Capability;
use uxduck_core::domain::operation::{Operation, Span, chains};
use uxduck_core::events::{Actor, Change};

use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// The longest name an operation takes.
const MAX_NAME_CHARS: usize = 120;

fn simulation_dto(
    simulation: &uxduck_core::domain::operation::Simulation,
) -> uxduck_contracts::dto::operation::Simulation {
    use uxduck_contracts::dto::operation::{RunState as Dto, SimulationRun};
    use uxduck_core::domain::operation::RunState;
    uxduck_contracts::dto::operation::Simulation {
        runs: simulation
            .runs
            .iter()
            .map(|run| SimulationRun {
                link_id: run.link_id.clone(),
                vehicle_name: run.vehicle_name.clone(),
                plan_id: run.plan_id.clone(),
                plan_name: run.plan_name.clone(),
                speedup: run.speedup,
                wind_mps: run.wind_mps,
                wind_from_deg: run.wind_from_deg,
                state: match run.state {
                    RunState::Starting => Dto::Starting,
                    RunState::Uploading => Dto::Uploading,
                    RunState::Flying => Dto::Flying,
                    RunState::Finished => Dto::Finished,
                    RunState::EndedEarly => Dto::EndedEarly,
                    RunState::TimedOut => Dto::TimedOut,
                    RunState::Failed => Dto::Failed,
                    RunState::Stopped => Dto::Stopped,
                },
                preparing: run.preparing,
                detail: run.detail.clone(),
                flight_id: run.flight_id.clone(),
                breached: run.breached,
            })
            .collect(),
        recommended: simulation.recommended,
        limit_min: simulation.limit_min,
    }
}

fn grouping() -> Actor {
    Actor {
        client_id: "operations".into(),
        operator_id: None,
        identity: None,
    }
}

impl Engine {
    /// An operation as clients see it: its vehicles and kept flights counted.
    pub(crate) fn operation_dto(&self, operation: &Operation) -> OperationDto {
        let flights: Vec<_> = operation
            .flight_ids
            .iter()
            .filter_map(|id| self.state.flights.get(id))
            .collect();
        let vehicles: BTreeSet<&str> = flights.iter().map(|f| f.vehicle_id.as_str()).collect();
        OperationDto {
            id: operation.id.clone(),
            name: operation.name.clone(),
            auto: operation.auto,
            flight_ids: operation.flight_ids.clone(),
            from_us: operation.from_us,
            to_us: operation.to_us,
            vehicles: u32::try_from(vehicles.len()).unwrap_or(u32::MAX),
            kept: u32::try_from(flights.iter().filter(|f| f.keep).count()).unwrap_or(u32::MAX),
            video_ids: operation.video_ids.clone(),
            clock_offsets: operation
                .clock_offsets_us
                .iter()
                .map(|(flight_id, &offset_us)| {
                    uxduck_contracts::dto::operation::FlightClockOffset {
                        flight_id: flight_id.clone(),
                        offset_us,
                    }
                })
                .collect(),
            simulation: operation.simulation.as_ref().map(simulation_dto),
            version: operation.version,
        }
    }

    /// Its span and kept flag from its flights, oldest first.
    fn settle_operation(&self, operation: &mut Operation) {
        let mut flights: Vec<_> = operation
            .flight_ids
            .iter()
            .filter_map(|id| self.state.flights.get(id))
            .collect();
        flights.sort_by_key(|f| (f.armed_at_us, f.id.clone()));
        operation.flight_ids = flights.iter().map(|f| f.id.clone()).collect();
        let ids = &operation.flight_ids;
        operation.clock_offsets_us.retain(|id, _| ids.contains(id));
        if let (Some(first), Some(_)) = (flights.first(), flights.last()) {
            operation.from_us = first.armed_at_us;
            operation.to_us = flights
                .iter()
                .map(|f| f.ended_at_us.unwrap_or(f.armed_at_us))
                .max()
                .unwrap_or(first.armed_at_us);
        }
        operation.keep = flights.iter().any(|f| f.keep);
    }

    /// The flights a named replay saved keep its name (ADR-0039 R21): the
    /// operation holding them all is named, or one is made of them.
    pub(crate) fn name_saved_flights(&mut self, actor: Actor, name: &str, flight_ids: &[String]) {
        if flight_ids.is_empty() {
            return;
        }
        let holding = self
            .state
            .operations
            .values()
            .find(|o| flight_ids.iter().all(|f| o.flight_ids.contains(f)))
            .cloned();
        let operation = if let Some(mut operation) = holding {
            operation.name = Some(name.to_owned());
            operation.auto = false;
            operation
        } else {
            let id = format!("op{}", &self.random.token()[..16]);
            self.take_flights_from_others(&actor, flight_ids, &id);
            Operation {
                id,
                name: Some(name.to_owned()),
                auto: false,
                flight_ids: flight_ids.to_vec(),
                from_us: self.clock.now_us(),
                to_us: self.clock.now_us(),
                video_ids: Vec::new(),
                clock_offsets_us: std::collections::BTreeMap::new(),
                keep: false,
                simulation: None,
                version: 0,
            }
        };
        self.put_operation(actor, operation);
        self.regroup_operations();
    }

    /// An operation renamed from its replay (ADR-0039 R21), as `operation/rename`.
    pub(crate) fn rename_operation(&mut self, connection: ConnectionId, operation: Operation) {
        let actor = self.actor(connection);
        self.put_operation(actor, operation);
    }

    pub(crate) fn put_operation(&mut self, actor: Actor, mut operation: Operation) -> Operation {
        self.settle_operation(&mut operation);
        let previous = self.state.operations.get(&operation.id);
        if previous.is_some_and(|p| {
            let mut same = operation.clone();
            same.version = p.version;
            &same == p
        }) {
            return operation;
        }
        operation.version = previous.map_or(1, |p| p.version + 1);
        self.commit_as(
            actor,
            Change::OperationUpserted {
                operation: operation.clone(),
            },
        );
        operation
    }

    /// Groups the ended flights again: each chain of two or more vehicles
    /// is an operation made by itself, the one that shares its flights
    /// kept; flights in a person's operation stay there.
    pub(crate) fn regroup_operations(&mut self) {
        let taken: BTreeSet<String> = self
            .state
            .operations
            .values()
            .filter(|o| !o.auto)
            .flat_map(|o| o.flight_ids.iter().cloned())
            .collect();
        let spans: Vec<Span<'_>> = self
            .state
            .flights
            .values()
            .filter(|f| !taken.contains(&f.id))
            .filter_map(|f| {
                Some(Span {
                    id: &f.id,
                    vehicle_id: &f.vehicle_id,
                    from_us: f.armed_at_us,
                    to_us: f.ended_at_us?,
                })
            })
            .collect();
        let found: Vec<Vec<String>> = chains(spans)
            .into_iter()
            .map(|chain| chain.into_iter().map(|f| f.id.to_owned()).collect())
            .collect();
        let mut autos: BTreeMap<String, BTreeSet<String>> = self
            .state
            .operations
            .values()
            .filter(|o| o.auto)
            .map(|o| (o.id.clone(), o.flight_ids.iter().cloned().collect()))
            .collect();
        let mut wanted = Vec::new();
        for flights in found {
            // The operation made by itself that shares the most of them.
            let matched = autos
                .iter()
                .map(|(id, had)| (flights.iter().filter(|f| had.contains(*f)).count(), id))
                .filter(|(shared, _)| *shared > 0)
                .max()
                .map(|(_, id)| id.clone());
            let operation = match matched.and_then(|id| {
                autos.remove(&id);
                self.state.operations.get(&id).cloned()
            }) {
                Some(mut operation) => {
                    operation.flight_ids = flights;
                    operation
                }
                None => Operation {
                    id: format!("op{}", &self.random.token()[..16]),
                    name: None,
                    auto: true,
                    flight_ids: flights,
                    from_us: 0,
                    to_us: 0,
                    video_ids: Vec::new(),
                    clock_offsets_us: std::collections::BTreeMap::new(),
                    keep: false,
                    simulation: None,
                    version: 0,
                },
            };
            wanted.push(operation);
        }
        for operation in wanted {
            self.put_operation(grouping(), operation);
        }
        // Made by itself and no longer two vehicles: gone.
        for id in autos.into_keys() {
            self.commit_as(grouping(), Change::OperationRemoved { id });
        }
        // A person's operations follow their flights being kept or deleted.
        let people: Vec<Operation> = self
            .state
            .operations
            .values()
            .filter(|o| !o.auto)
            .cloned()
            .collect();
        for operation in people {
            if !operation.flight_ids.is_empty()
                && operation
                    .flight_ids
                    .iter()
                    .all(|id| !self.state.flights.contains_key(id))
            {
                self.commit_as(grouping(), Change::OperationRemoved { id: operation.id });
            } else {
                self.put_operation(grouping(), operation);
            }
        }
    }

    fn known_flights(&self, ids: &[String]) -> Result<Vec<String>, ApiError> {
        let mut out = Vec::new();
        for id in ids {
            if !self.state.flights.contains_key(id) {
                return Err(ApiError::new(
                    ErrorKind::NotFound,
                    format!("no flight {id}"),
                ));
            }
            if !out.contains(id) {
                out.push(id.clone());
            }
        }
        Ok(out)
    }

    /// Takes flights out of every operation but `keep_in`; one left with
    /// none goes, one made by itself is regrouped after.
    pub(crate) fn take_flights_from_others(
        &mut self,
        actor: &Actor,
        flights: &[String],
        keep_in: &str,
    ) {
        let others: Vec<Operation> = self
            .state
            .operations
            .values()
            .filter(|o| o.id != keep_in && o.flight_ids.iter().any(|f| flights.contains(f)))
            .cloned()
            .collect();
        for mut operation in others {
            operation.flight_ids.retain(|f| !flights.contains(f));
            if operation.flight_ids.is_empty() {
                self.commit_as(actor.clone(), Change::OperationRemoved { id: operation.id });
            } else {
                self.put_operation(actor.clone(), operation);
            }
        }
    }

    fn operation_named(&self, id: &str) -> Result<Operation, ApiError> {
        self.state
            .operations
            .get(id)
            .cloned()
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no operation {id}")))
    }

    /// `operation/list`, newest first.
    #[must_use]
    pub fn operations(&self) -> OperationListResult {
        let mut operations: Vec<OperationDto> = self
            .state
            .operations
            .values()
            .map(|o| self.operation_dto(o))
            .collect();
        operations.sort_by(|a, b| b.from_us.cmp(&a.from_us).then(a.id.cmp(&b.id)));
        OperationListResult { operations }
    }
}

fn name_of(name: Option<&str>) -> Result<Option<String>, ApiError> {
    let name = name.map(str::trim).filter(|n| !n.is_empty());
    if name.is_some_and(|n| n.chars().count() > MAX_NAME_CHARS) {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!("a name is at most {MAX_NAME_CHARS} characters"),
        ));
    }
    Ok(name.map(str::to_owned))
}

/// Every `operation/*` method.
pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    request: ClientRequest,
    granted: &[Capability],
) -> Result<Value, ApiError> {
    let needed = if matches!(request, ClientRequest::OperationList(_)) {
        Capability::Read
    } else {
        Capability::Config
    };
    if !granted.contains(&needed) {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            needed.denied_message(),
        ));
    }
    let actor = engine.actor(connection);
    match request {
        ClientRequest::OperationList(Empty {}) => to_value(&engine.operations()),
        ClientRequest::OperationCreate(OperationCreateParams { name, flight_ids }) => {
            let flights = engine.known_flights(&flight_ids)?;
            let id = format!("op{}", &engine.random.token()[..16]);
            engine.take_flights_from_others(&actor, &flights, &id);
            let operation = Operation {
                id,
                name: name_of(name.as_deref())?,
                auto: false,
                flight_ids: flights,
                from_us: engine.clock.now_us(),
                to_us: engine.clock.now_us(),
                video_ids: Vec::new(),
                clock_offsets_us: std::collections::BTreeMap::new(),
                keep: false,
                simulation: None,
                version: 0,
            };
            let operation = engine.put_operation(actor, operation);
            engine.regroup_operations();
            to_value(&engine.operation_dto(&operation))
        }
        ClientRequest::OperationRename(OperationRenameParams { id, name }) => {
            let mut operation = engine.operation_named(&id)?;
            operation.name = name_of(Some(&name))?;
            operation.auto = false;
            let operation = engine.put_operation(actor, operation);
            to_value(&engine.operation_dto(&operation))
        }
        ClientRequest::OperationAddFlights(OperationFlightsParams { id, flight_ids }) => {
            let mut operation = engine.operation_named(&id)?;
            let flights = engine.known_flights(&flight_ids)?;
            engine.take_flights_from_others(&actor, &flights, &id);
            for flight in flights {
                if !operation.flight_ids.contains(&flight) {
                    operation.flight_ids.push(flight);
                }
            }
            operation.auto = false;
            let operation = engine.put_operation(actor, operation);
            engine.regroup_operations();
            to_value(&engine.operation_dto(&operation))
        }
        ClientRequest::OperationRemoveFlights(OperationFlightsParams { id, flight_ids }) => {
            let mut operation = engine.operation_named(&id)?;
            operation.flight_ids.retain(|f| !flight_ids.contains(f));
            operation.auto = false;
            if operation.flight_ids.is_empty() {
                engine.commit_as(actor, Change::OperationRemoved { id });
            } else {
                engine.put_operation(actor, operation);
            }
            engine.regroup_operations();
            to_value(&engine.operations())
        }
        _ => Err(ApiError::new(
            ErrorKind::Internal,
            "not an operation method",
        )),
    }
}

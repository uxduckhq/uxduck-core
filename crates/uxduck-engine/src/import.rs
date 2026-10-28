//! Taking in a browser's work (ADR-0011 "Moving from the browser to an
//! installed daemon", S20). The browser sends its operators, chats with
//! their messages, folders, saved places and simulation snapshots (S34) as
//! the changes that create them. Each is taken only if it is new here or
//! newer than the copy here, so importing the same work twice changes
//! nothing; every change taken is recorded as coming from the browser.
//! Simulated vehicles, their links and anything else stay where they ran.
//! So does the stand-in operator the page made for a browser with none,
//! when UXDuck here already has an operator: a browser's work moved in
//! after each visit without an installed UXDuck would add one every time.
//! What it owned (chats, folders, places) becomes the operator moving it in,
//! or everyone's: never an operator who is not here, as whom nobody could
//! see it (S167).

use std::collections::BTreeSet;

use serde_json::Value;
use uxduck_contracts::dto::initialize::RunsIn;
use uxduck_contracts::dto::journal::{JournalImportParams, JournalImportResult};
use uxduck_contracts::dto::operator::DEFAULT_OPERATOR_NAME;
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::events::{Actor, Change};

use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// The longest `source` kept on the changes.
const MAX_SOURCE: usize = 100;

pub(crate) fn handle(
    engine: &mut Engine,
    connection: ConnectionId,
    params: JournalImportParams,
    operator: bool,
) -> Result<Value, ApiError> {
    if !operator {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "only an operator client may move work into UXDuck",
        ));
    }
    if engine.config.capabilities.runs_in == RunsIn::Browser {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "only UXDuck Desktop takes in work from elsewhere",
        ));
    }
    let source = params.source.trim();
    if source.is_empty() || source.len() > MAX_SOURCE {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            "an import needs a short source, such as the browser's id",
        ));
    }
    let actor = Actor {
        client_id: format!("import:{source}"),
        operator_id: None,
        identity: None,
    };
    let mut result = JournalImportResult {
        imported: 0,
        skipped: 0,
    };
    let mover = engine
        .connections
        .get(&connection)
        .and_then(|c| c.operator_id.clone());
    // Stand-ins left behind; an operator comes before what it owns.
    let mut left_behind = BTreeSet::new();
    for value in params.changes {
        let taken = serde_json::from_value::<Change>(value).is_ok_and(|change| {
            engine.import_change(&actor, change, &mut left_behind, mover.as_deref())
        });
        if taken {
            result.imported += 1;
        } else {
            result.skipped += 1;
        }
    }
    to_value(&result)
}

impl Engine {
    /// One change from elsewhere, taken if its entity moves and is new here
    /// or newer. Returns whether it was taken.
    fn import_change(
        &mut self,
        actor: &Actor,
        mut change: Change,
        left_behind: &mut BTreeSet<String>,
        mover: Option<&str>,
    ) -> bool {
        // The page's stand-in stays behind when there is an operator here
        // already, unless it is the same one (moved in before).
        if let Change::OperatorUpserted { operator } = &change
            && operator.name == DEFAULT_OPERATOR_NAME
            && !self.state.operators.is_empty()
            && !self.state.operators.contains_key(&operator.id)
        {
            left_behind.insert(operator.id.clone());
            return false;
        }
        let owner = match &mut change {
            Change::ThreadUpserted { thread } => Some(&mut thread.operator_id),
            Change::FolderUpserted { folder } => Some(&mut folder.operator_id),
            Change::PlaceUpserted { place } => Some(&mut place.operator_id),
            _ => None,
        };
        if let Some(owner) = owner
            && owner.as_ref().is_some_and(|id| left_behind.contains(id))
        {
            *owner = mover.map(str::to_owned);
        }
        let newer = |here: Option<u64>, there: u64| here.is_none_or(|version| version < there);
        let state = &self.state;
        let take = match &change {
            Change::OperatorUpserted { operator } => newer(
                state.operators.get(&operator.id).map(|o| o.version),
                operator.version,
            ),
            Change::ThreadUpserted { thread } => newer(
                state.threads.get(&thread.id).map(|t| t.version),
                thread.version,
            ),
            // A message moves with its chat, which comes first.
            Change::ItemUpserted { item } => {
                state.threads.contains_key(&item.thread_id)
                    && newer(state.items.get(&item.id).map(|i| i.version), item.version)
            }
            Change::FolderUpserted { folder } => newer(
                state.folders.get(&folder.id).map(|f| f.version),
                folder.version,
            ),
            Change::PlaceUpserted { place } => newer(
                state.places.get(&place.id).map(|p| p.version),
                place.version,
            ),
            Change::PlanSaved { plan, .. } => {
                newer(state.plans.get(&plan.id).map(|p| p.version), plan.version)
            }
            Change::SnapshotUpserted { snapshot } => newer(
                state.snapshots.get(&snapshot.id).map(|s| s.version),
                snapshot.version,
            ),
            _ => false,
        };
        if take {
            self.commit_as(actor.clone(), change);
        }
        take
    }
}

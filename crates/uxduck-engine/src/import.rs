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

use serde_json::Value;
use uxduck_contracts::dto::initialize::RunsIn;
use uxduck_contracts::dto::journal::{JournalImportParams, JournalImportResult};
use uxduck_contracts::dto::operator::DEFAULT_OPERATOR_NAME;
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::events::{Actor, Change};

use crate::Engine;
use crate::rpc::handlers::to_value;

/// The longest `source` kept on the changes.
const MAX_SOURCE: usize = 100;

pub(crate) fn handle(
    engine: &mut Engine,
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
    for value in params.changes {
        let taken = serde_json::from_value::<Change>(value)
            .is_ok_and(|change| engine.import_change(&actor, change));
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
    fn import_change(&mut self, actor: &Actor, change: Change) -> bool {
        let newer = |here: Option<u64>, there: u64| here.is_none_or(|version| version < there);
        let state = &self.state;
        let take = match &change {
            // The page's stand-in stays behind when there is an operator
            // here already, unless it is the same one (moved in before).
            Change::OperatorUpserted { operator }
                if operator.name == DEFAULT_OPERATOR_NAME
                    && !state.operators.is_empty()
                    && !state.operators.contains_key(&operator.id) =>
            {
                false
            }
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

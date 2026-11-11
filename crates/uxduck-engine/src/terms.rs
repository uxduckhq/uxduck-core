//! Agreeing to the Terms of Service (ADR-0047). Each operator agrees once
//! per terms version, recorded on them, and UXDuck arms a real vehicle only
//! for an operator who has. Agreeing is a person's: only an operator's
//! interface may send `terms/accept`, so an agent never agrees for them.
//! When the computer is signed in, the account keeps a record too.

use serde_json::Value;
use uxduck_contracts::dto::operator::{Operator as OperatorDto, TermsAcceptParams};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_contracts::version::TERMS_VERSION;
use uxduck_core::domain::command::{Receipt, ReceiptStatus, RefusalReason};
use uxduck_core::domain::operator::{Operator, TermsAcceptance};
use uxduck_core::domain::vehicle::Vehicle;
use uxduck_core::events::Change;

use crate::account::AccountAction;
use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// Why a real vehicle was not armed, for the receipt.
pub const NOT_AGREED: &str =
    "Agree to UXDuck's Terms of Service before UXDuck arms a real vehicle.";

pub(crate) fn accept(
    engine: &mut Engine,
    connection: ConnectionId,
    params: &TermsAcceptParams,
) -> Result<Value, ApiError> {
    if params.version != TERMS_VERSION {
        return Err(ApiError::new(
            ErrorKind::InvalidValue,
            format!(
                "These are not the current terms ({TERMS_VERSION}). Reload UXDuck to read them."
            ),
        ));
    }
    let id = engine
        .connections
        .get(&connection)
        .and_then(|c| c.operator_id.clone())
        .ok_or_else(|| {
            ApiError::new(
                ErrorKind::InvalidValue,
                "Choose a local operator before agreeing to the terms.",
            )
        })?;
    let mut operator = engine
        .state
        .operators
        .get(&id)
        .cloned()
        .ok_or_else(|| ApiError::new(ErrorKind::NotFound, format!("no operator {id}")))?;
    let account_id = engine
        .account_identity
        .signed_in
        .then(|| engine.account_identity.account_id.clone())
        .flatten();
    operator.terms = Some(TermsAcceptance {
        version: TERMS_VERSION.to_owned(),
        accepted_at_ms: engine.clock.now_us() / 1000,
        account_id: account_id.clone(),
    });
    operator.version += 1;
    let result = OperatorDto::from(&operator);
    engine.commit(connection, Change::OperatorUpserted { operator });
    // The account's record goes on its own: the answer is the operator's,
    // kept whether or not the server can be reached.
    if account_id.is_some() && engine.account.is_some() {
        let answering = engine.replying_to.take();
        engine.ask_account(AccountAction::AcceptTerms {
            version: TERMS_VERSION.to_owned(),
            context: params.context,
        });
        engine.replying_to = answering;
    }
    to_value(&result)
}

fn current(operator: &Operator) -> bool {
    operator
        .terms
        .as_ref()
        .is_some_and(|terms| terms.version == TERMS_VERSION)
}

impl Engine {
    /// Whether an operator has agreed to the current terms. A sender with
    /// no operator, such as an agent's or a plugin's connection, acts for
    /// the people at this computer: one of them must have.
    pub(crate) fn agreed_to_terms(&self, operator_id: Option<&str>) -> bool {
        match operator_id {
            Some(id) => self.state.operators.get(id).is_some_and(current),
            None => self.state.operators.values().any(current),
        }
    }

    /// Holds a command that would arm a real vehicle on the ground until
    /// its sender's operator has agreed to the current terms: `not-sent`
    /// with reason `terms`. Nothing else: telemetry, disarming, landing,
    /// returning and mode changes go as they would, so it never stands
    /// between someone and a vehicle already flying.
    pub(crate) fn terms_gate(&self, vehicle: &Vehicle, arms: bool, receipt: &mut Receipt) {
        if receipt.status != ReceiptStatus::Sent
            || !arms
            || vehicle.armed
            || vehicle.replay
            || self.is_simulated(vehicle)
            || self.agreed_to_terms(receipt.operator_id.as_deref())
        {
            return;
        }
        receipt.status = ReceiptStatus::NotSent;
        receipt.detail = Some(NOT_AGREED.into());
        receipt.reason = Some(RefusalReason::Terms);
        receipt.completed_at_us = Some(receipt.created_at_us);
    }
}

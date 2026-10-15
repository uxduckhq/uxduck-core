//! Feedback on the agent's answers (S29, ADR-0013 P13). A thumbs up or down
//! is kept on the message here, then sent: with the kept turn when the
//! hosted model kept it, or, when the operator agrees after a thumbs-down,
//! with the conversation itself. Nothing is sent without an account.

use serde_json::{Value, json};
use uxduck_contracts::dto::thread::{
    Rating as RatingDto, ThreadFeedbackParams, ThreadFeedbackResult,
};
use uxduck_contracts::errors::{ApiError, ErrorKind};
use uxduck_core::domain::thread::{Feedback, Item, ItemKind, Rating};
use uxduck_core::events::{Actor, Change};

use crate::account::AccountAction;
use crate::rpc::handlers::to_value;
use crate::{ConnectionId, Engine};

/// The longest comment kept.
const MAX_COMMENT: usize = 2_000;

/// A message as the API takes it with a sent conversation.
fn sent_item(item: &Item) -> Value {
    let parsed = |text: &str| serde_json::from_str::<Value>(text).unwrap_or_else(|_| json!(text));
    let mut value = json!({ "kind": item.kind, "text": item.text });
    if let Some(call) = &item.tool_call {
        value["toolCall"] = json!({
            "method": call.method,
            "params": parsed(&call.params),
            "result": call.result.as_deref().map(parsed),
            "error": call.error.as_deref().map(parsed),
        });
    }
    value
}

pub(crate) fn feedback(
    engine: &mut Engine,
    connection: ConnectionId,
    params: ThreadFeedbackParams,
    operator: bool,
) -> Result<Value, ApiError> {
    if !operator {
        return Err(ApiError::new(
            ErrorKind::CapabilityDenied,
            "only an operator client rates the agent's answers",
        ));
    }
    let item = engine
        .state
        .items
        .get(&params.item_id)
        .filter(|item| item.kind == ItemKind::AgentMessage)
        .cloned()
        .ok_or_else(|| ApiError::new(ErrorKind::NotFound, "no such answer"))?;
    let comment = params
        .comment
        .map(|c| c.trim().chars().take(MAX_COMMENT).collect::<String>())
        .filter(|c| !c.is_empty());
    let rating = match params.rating {
        RatingDto::Up => Rating::Up,
        RatingDto::Down => Rating::Down,
    };
    let mut rated = item.clone();
    rated.feedback = Some(Feedback {
        rating,
        comment: comment.clone(),
        sent: item
            .feedback
            .as_ref()
            .is_some_and(|f| f.sent && f.rating == rating),
    });
    rated.version += 1;
    engine.commit(connection, Change::ItemUpserted { item: rated });

    let rating = match rating {
        Rating::Up => "up",
        Rating::Down => "down",
    };
    let action = if params.send {
        let thread = engine.state.threads.get(&item.thread_id);
        let items: Vec<Value> = engine
            .state
            .items_of(&item.thread_id)
            .into_iter()
            .filter(|i| i.index <= item.index)
            .map(sent_item)
            .collect();
        AccountAction::SendConversation {
            thread_id: item.thread_id.clone(),
            model: thread.map(|t| t.model.clone()).unwrap_or_default(),
            items,
            rating,
            comment,
        }
    } else if let Some(response_id) = item.response_id.clone() {
        AccountAction::Feedback {
            response_id,
            rating,
            comment,
        }
    } else {
        return to_value(&ThreadFeedbackResult {
            sent: false,
            not_shared: true,
        });
    };
    if engine.account.is_none() {
        return to_value(&ThreadFeedbackResult {
            sent: false,
            not_shared: false,
        });
    }
    let order = engine.ask_account(action);
    engine.awaiting_feedback_items.insert(order, item.id);
    Ok(Value::Null)
}

impl Engine {
    /// The feedback on an answer reached UXDuck.
    pub(crate) fn feedback_sent(&mut self, item_id: &str) {
        let Some(mut item) = self.state.items.get(item_id).cloned() else {
            return;
        };
        let Some(feedback) = item.feedback.as_mut() else {
            return;
        };
        feedback.sent = true;
        item.version += 1;
        self.commit_as(Actor::default(), Change::ItemUpserted { item });
    }
}

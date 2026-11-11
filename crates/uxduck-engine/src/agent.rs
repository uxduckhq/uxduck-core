//! The engine's side of the agent host (ADR-0009, local host). The host runs
//! turns outside the engine and reports what happens through these methods;
//! each report is journaled and pushed like any other change, except the
//! streamed text of a message in progress, which goes out as
//! `agent/itemDelta` and is kept in memory until the message completes.

use uxduck_contracts::dto::state::EntityType;
use uxduck_contracts::dto::thread::AgentItemDelta;
use uxduck_contracts::rpc::ServerNotification;
use uxduck_core::domain::thread::{Attachment, Item, ItemKind, ItemStatus, Spoken, ToolCall};
use uxduck_core::events::{Actor, Change};

use crate::{ConnectionId, Engine, rpc};

/// How a tool call ended, as JSON text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolOutcome {
    Result(String),
    /// The `ApiError`, as JSON text.
    Error(String),
}

impl Engine {
    #[must_use]
    pub fn agent_title_requested(&self, thread_id: &str, item_id: &str) -> bool {
        use uxduck_core::domain::thread::TitleState;

        self.state.threads.get(thread_id).is_some_and(|thread| {
            thread.deleted_at_us.is_none()
                && thread.title_state
                    == (TitleState::Requested {
                        item_id: item_id.to_owned(),
                    })
        }) && self.state.items.get(item_id).is_some_and(|item| {
            item.thread_id == thread_id && item.kind == ItemKind::UserMessage && item.in_history()
        })
    }

    pub fn agent_title_finish(&mut self, thread_id: &str, item_id: &str, title: Option<&str>) {
        use uxduck_core::domain::thread::{MAX_TITLE_CHARS, TitleState};

        if !self.agent_title_requested(thread_id, item_id) {
            return;
        }
        let Some(mut thread) = self.state.threads.get(thread_id).cloned() else {
            return;
        };
        thread.title_state = TitleState::Fixed;
        if let Some(title) = title.map(str::trim).filter(|title| {
            !title.is_empty()
                && title.chars().count() <= MAX_TITLE_CHARS
                && !title
                    .chars()
                    .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
        }) {
            title.clone_into(&mut thread.title);
        }
        thread.version += 1;
        self.commit_as(Actor::default(), Change::ThreadUpserted { thread });
    }

    /// Appends an item to a thread; the new item's id.
    pub(crate) fn add_item(
        &mut self,
        actor: Actor,
        thread_id: &str,
        kind: ItemKind,
        status: ItemStatus,
        text: String,
        tool_call: Option<ToolCall>,
    ) -> String {
        self.add_message(
            actor,
            thread_id,
            kind,
            status,
            text,
            tool_call,
            Vec::new(),
            None,
        )
    }

    /// As `add_item`, with what the operator attached (ADR-0020 H2) and
    /// whether they said it aloud (ADR-0012 V4).
    #[allow(clippy::too_many_arguments, reason = "an item's fields")]
    pub(crate) fn add_message(
        &mut self,
        actor: Actor,
        thread_id: &str,
        kind: ItemKind,
        status: ItemStatus,
        text: String,
        tool_call: Option<ToolCall>,
        attachments: Vec<Attachment>,
        spoken: Option<Spoken>,
    ) -> String {
        let index = self
            .state
            .items_of(thread_id)
            .last()
            .map_or(0, |last| last.index + 1);
        let item = Item {
            id: format!("it{}", &self.random.token()[..16]),
            thread_id: thread_id.to_owned(),
            index,
            kind,
            status,
            text,
            tool_call,
            created_at_us: self.clock.now_us(),
            version: 1,
            response_id: None,
            feedback: None,
            attachments,
            spoken,
        };
        let id = item.id.clone();
        self.commit_as(actor, Change::ItemUpserted { item });
        self.touch_thread(thread_id, None);
        id
    }

    /// Bumps a thread's `updatedAt` (and its provider conversation id).
    pub(crate) fn touch_thread(&mut self, thread_id: &str, response_id: Option<String>) {
        let Some(mut thread) = self.state.threads.get(thread_id).cloned() else {
            return;
        };
        thread.updated_at_us = self.clock.now_us();
        if response_id.is_some() {
            thread.last_response_id = response_id;
        }
        thread.version += 1;
        self.commit_as(Actor::default(), Change::ThreadUpserted { thread });
    }

    pub(crate) fn agent_actor(&self, agent: Option<ConnectionId>) -> Actor {
        agent.map_or_else(
            || Actor {
                client_id: "agent".into(),
                operator_id: None,
                identity: None,
            },
            |id| self.actor(id),
        )
    }

    /// Starts an agent message; text follows through [`Engine::agent_text_delta`].
    pub fn agent_message_start(&mut self, agent: Option<ConnectionId>, thread_id: &str) -> String {
        let actor = self.agent_actor(agent);
        let id = self.add_item(
            actor,
            thread_id,
            ItemKind::AgentMessage,
            ItemStatus::InProgress,
            String::new(),
            None,
        );
        self.streaming.insert(id.clone(), String::new());
        id
    }

    /// Streams text into a message in progress.
    pub fn agent_text_delta(&mut self, item_id: &str, delta: &str) {
        let Some(text) = self.streaming.get_mut(item_id) else {
            return;
        };
        text.push_str(delta);
        let Some(thread_id) = self
            .state
            .items
            .get(item_id)
            .map(|item| item.thread_id.clone())
        else {
            return;
        };
        let thread_id_for_check = thread_id.clone();
        let frame =
            rpc::router::notification(&ServerNotification::AgentItemDelta(AgentItemDelta {
                thread_id,
                item_id: item_id.to_owned(),
                delta: delta.to_owned(),
            }));
        let subscribers: Vec<ConnectionId> = self
            .subscribers
            .iter()
            .filter(|(id, types)| {
                types.contains(&EntityType::Item)
                    && rpc::threads::sees_thread(self, **id, &thread_id_for_check)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in subscribers {
            self.send(id, &frame);
        }
    }

    /// Ends a message with its final status; the whole text is journaled,
    /// with the provider's response id that feedback on it refers to.
    pub fn agent_message_finish(
        &mut self,
        agent: Option<ConnectionId>,
        item_id: &str,
        status: ItemStatus,
        response_id: Option<String>,
    ) {
        let text = self.streaming.remove(item_id).unwrap_or_default();
        self.finish_item(agent, item_id, |item| {
            item.status = status;
            item.text = text;
            item.response_id = response_id;
        });
    }

    /// Records a tool call the agent is about to make on `connection`.
    pub fn agent_tool_start(
        &mut self,
        agent: ConnectionId,
        thread_id: &str,
        method: &str,
        params: String,
    ) -> String {
        let actor = self.actor(agent);
        let tool_call = ToolCall {
            method: method.to_owned(),
            params,
            client_id: actor.client_id.clone(),
            result: None,
            error: None,
        };
        self.add_item(
            actor,
            thread_id,
            ItemKind::ToolCall,
            ItemStatus::InProgress,
            String::new(),
            Some(tool_call),
        )
    }

    /// Records the receipt of a tool call.
    pub fn agent_tool_finish(&mut self, agent: ConnectionId, item_id: &str, outcome: ToolOutcome) {
        self.finish_item(Some(agent), item_id, |item| {
            let failed = matches!(outcome, ToolOutcome::Error(_));
            if let Some(call) = item.tool_call.as_mut() {
                match outcome {
                    ToolOutcome::Result(result) => call.result = Some(result),
                    ToolOutcome::Error(error) => call.error = Some(error),
                }
            }
            item.status = if failed {
                ItemStatus::Failed
            } else {
                ItemStatus::Completed
            };
        });
    }

    /// Records why a turn could not run, such as a missing key.
    pub fn agent_error(&mut self, agent: Option<ConnectionId>, thread_id: &str, message: &str) {
        self.end_open_items(thread_id, ItemStatus::Failed);
        let actor = self.agent_actor(agent);
        let mut completed: Vec<_> = self
            .state
            .items_of(thread_id)
            .into_iter()
            .rev()
            .take_while(|item| item.kind != ItemKind::UserMessage)
            .filter(|item| item.kind == ItemKind::ToolCall && item.status == ItemStatus::Completed)
            .filter_map(|item| item.tool_call.as_ref().map(|call| call.method.as_str()))
            .take(8)
            .collect();
        completed.reverse();
        let text = if completed.is_empty() {
            message.to_owned()
        } else {
            format!(
                "{message}\n\nRecent tool calls completed before this error: {}. Their results are saved above. Completed actions were not undone. Check the current state before retrying.",
                completed.join(", ")
            )
        };
        self.add_item(
            actor,
            thread_id,
            ItemKind::Error,
            ItemStatus::Failed,
            text,
            None,
        );
    }

    /// Turn `turn` is over. Anything it left in progress ends as cancelled.
    /// A late report from a turn that was already cancelled and replaced
    /// changes nothing.
    pub fn agent_turn_finish(&mut self, thread_id: &str, turn: u64, response_id: Option<String>) {
        let success = response_id.is_some();
        self.agent_turn_complete(thread_id, turn, response_id, success);
    }

    /// Runtime-backed providers have a terminal result without a Responses id.
    pub fn agent_turn_complete(
        &mut self,
        thread_id: &str,
        turn: u64,
        response_id: Option<String>,
        success: bool,
    ) {
        if self.turns.get(thread_id) != Some(&turn) {
            return;
        }
        self.end_open_items(thread_id, ItemStatus::Cancelled);
        self.turns.remove(thread_id);
        self.turn_ended(thread_id, turn, false);
        self.touch_thread(thread_id, response_id);
        rpc::thread_queue::hold_steering(self, thread_id);
        if success {
            rpc::thread_queue::start_next(self, thread_id, None);
        }
    }

    /// Commit only input actually included in a model request. Late turns cannot consume it.
    pub fn agent_input_applied(&mut self, thread_id: &str, turn: u64, item_id: &str) {
        if self.turns.get(thread_id) != Some(&turn) {
            return;
        }
        let Some(mut item) = self
            .state
            .items
            .get(item_id)
            .filter(|i| i.thread_id == thread_id && i.status == ItemStatus::Steering)
            .cloned()
        else {
            return;
        };
        item.status = ItemStatus::Completed;
        item.version += 1;
        self.pending_origins.remove(item_id);
        self.commit_as(self.agent_actor(None), Change::ItemUpserted { item });
    }

    /// Whether a thread has a turn in progress.
    #[must_use]
    pub fn turn_in_progress(&self, thread_id: &str) -> bool {
        self.turns.contains_key(thread_id)
    }

    pub(crate) fn end_open_items(&mut self, thread_id: &str, status: ItemStatus) {
        let open: Vec<String> = self
            .state
            .items_of(thread_id)
            .into_iter()
            .filter(|item| item.status == ItemStatus::InProgress)
            .map(|item| item.id.clone())
            .collect();
        for id in open {
            let text = self.streaming.remove(&id);
            self.finish_item(None, &id, |item| {
                item.status = if item.kind == ItemKind::UserMessage {
                    ItemStatus::Completed
                } else {
                    status
                };
                if let Some(text) = text {
                    item.text = text;
                }
            });
        }
    }

    fn finish_item(
        &mut self,
        agent: Option<ConnectionId>,
        item_id: &str,
        change: impl FnOnce(&mut Item),
    ) {
        let Some(mut item) = self.state.items.get(item_id).cloned() else {
            return;
        };
        if item.status != ItemStatus::InProgress {
            return;
        }
        change(&mut item);
        item.version += 1;
        let actor = self.agent_actor(agent);
        self.commit_as(actor, Change::ItemUpserted { item });
    }
}

impl crate::Engine {
    /// Only public connection metadata crosses the engine/transport boundary.
    pub fn agent_connections(
        &mut self,
        connections: Vec<uxduck_contracts::dto::thread::AgentConnection>,
    ) {
        self.agent_connections = connections;
    }
}

impl crate::Engine {
    /// Disconnect stops active tasks before the provider's credentials are revoked.
    pub fn agent_provider_disconnected(&mut self, provider: &str) {
        let prefix = format!("{provider}:");
        let threads: Vec<String> = self
            .turns
            .keys()
            .filter(|id| {
                self.state
                    .threads
                    .get(*id)
                    .is_some_and(|t| t.model.starts_with(&prefix))
            })
            .cloned()
            .collect();
        for id in threads {
            let _ = crate::rpc::threads::cancel(self, &id);
        }
    }
}

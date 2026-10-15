use serde_json::Value;
use uxduck_core::domain::thread as core;

use crate::dto::thread as dto;
use crate::errors::ApiError;

impl From<&core::Thread> for dto::Thread {
    fn from(value: &core::Thread) -> Self {
        Self {
            id: value.id.clone(),
            title: value.title.clone(),
            operator_id: value.operator_id.clone(),
            vehicle_id: value.vehicle_id.clone(),
            model: value.model.clone(),
            auto_approve: value.auto_approve,
            created_at_us: value.created_at_us,
            updated_at_us: value.updated_at_us,
            version: value.version,
            folder_id: value.folder_id.clone(),
            sort_key: value.sort_key.clone(),
            pinned: value.pinned,
            archived: value.archived,
            exclude_from_learning: value.exclude_from_learning,
            deleted_at_us: value.deleted_at_us,
        }
    }
}

impl From<core::ItemKind> for dto::ItemKind {
    fn from(value: core::ItemKind) -> Self {
        match value {
            core::ItemKind::UserMessage => Self::UserMessage,
            core::ItemKind::AgentMessage => Self::AgentMessage,
            core::ItemKind::ToolCall => Self::ToolCall,
            core::ItemKind::Error => Self::Error,
        }
    }
}

impl From<core::ItemStatus> for dto::ItemStatus {
    fn from(value: core::ItemStatus) -> Self {
        match value {
            core::ItemStatus::InProgress => Self::InProgress,
            core::ItemStatus::Completed => Self::Completed,
            core::ItemStatus::Failed => Self::Failed,
            core::ItemStatus::Cancelled => Self::Cancelled,
            core::ItemStatus::Interrupted => Self::Interrupted,
        }
    }
}

/// Core keeps payloads as JSON text; anything unparseable is passed on as a string.
fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()))
}

impl From<&core::ToolCall> for dto::ToolCall {
    fn from(value: &core::ToolCall) -> Self {
        Self {
            method: value.method.clone(),
            params: json(&value.params),
            client_id: value.client_id.clone(),
            result: value.result.as_deref().map(json),
            error: value
                .error
                .as_deref()
                .and_then(|text| serde_json::from_str::<ApiError>(text).ok()),
        }
    }
}

impl From<&core::Item> for dto::Item {
    fn from(value: &core::Item) -> Self {
        Self {
            id: value.id.clone(),
            thread_id: value.thread_id.clone(),
            index: value.index,
            kind: value.kind.into(),
            status: value.status.into(),
            text: value.text.clone(),
            tool_call: value.tool_call.as_ref().map(Into::into),
            created_at_us: value.created_at_us,
            version: value.version,
            feedback: value.feedback.as_ref().map(|f| dto::Feedback {
                rating: match f.rating {
                    core::Rating::Up => dto::Rating::Up,
                    core::Rating::Down => dto::Rating::Down,
                },
                comment: f.comment.clone(),
                sent: f.sent,
            }),
            attachments: value.attachments.iter().map(Into::into).collect(),
        }
    }
}

impl From<&core::Attachment> for dto::Attachment {
    fn from(value: &core::Attachment) -> Self {
        Self {
            title: value.title.clone(),
            key: value.key.clone(),
            value: value.value.clone(),
            unit: value.unit.clone(),
            plugin: value.plugin.clone(),
            help: value.help.clone(),
        }
    }
}

impl From<&dto::Attachment> for core::Attachment {
    fn from(value: &dto::Attachment) -> Self {
        Self {
            title: value.title.clone(),
            key: value.key.clone(),
            value: value.value.clone(),
            unit: value.unit.clone(),
            plugin: value.plugin.clone(),
            help: value.help.clone(),
        }
    }
}

impl From<&core::Folder> for dto::Folder {
    fn from(value: &core::Folder) -> Self {
        Self {
            id: value.id.clone(),
            name: value.name.clone(),
            parent_id: value.parent_id.clone(),
            operator_id: value.operator_id.clone(),
            sort_key: value.sort_key.clone(),
            created_at_us: value.created_at_us,
            version: value.version,
            deleted_at_us: value.deleted_at_us,
        }
    }
}

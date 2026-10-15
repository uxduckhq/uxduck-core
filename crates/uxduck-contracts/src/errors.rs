//! `ApiError`: the only error that crosses the wire. Numeric codes are stable
//! and part of the published contract; `kind` is the same fact as a string for
//! people and logs.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Every error the daemon returns, with its stable code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorKind {
    /// The message is not valid JSON. JSON-RPC `-32700`.
    ParseError,
    /// Valid JSON but not a JSON-RPC request. `-32600`.
    InvalidRequest,
    /// Unknown method; the documented way to probe an older daemon. `-32601`.
    MethodNotFound,
    /// The params do not match the method's schema. `-32602`.
    InvalidParams,
    /// A daemon bug. `-32603`.
    Internal,
    /// A call other than `initialize` before `initialize`. `-32000`.
    NotInitialized,
    /// `initialize` a second time on the same connection. `-32001`.
    AlreadyInitialized,
    /// The client is too old or speaks another protocol version. `-32002`.
    UpdateRequired,
    /// The connection may not make this call (and cannot escalate yet). `-32003`.
    CapabilityDenied,
    /// No such entity, or no pending pairing with that code. `-32004`.
    NotFound,
    /// A write started from a version that is no longer current. `-32005`.
    /// The client already holds the newer delta.
    VersionConflict,
    /// A value the daemon will not store, such as an empty name. `-32006`.
    InvalidValue,
    /// The thread already has a turn in progress. `-32007`.
    Busy,
    /// Another program holds the port; the message names it when the
    /// platform says. `-32008`.
    PortInUse,
    /// The command needs `confirm: true` while the vehicle is not online
    /// (its offline behaviour is `warn`). `-32009`.
    ConfirmationRequired,
}

impl ErrorKind {
    #[must_use]
    pub const fn code(self) -> i32 {
        match self {
            Self::ParseError => -32700,
            Self::InvalidRequest => -32600,
            Self::MethodNotFound => -32601,
            Self::InvalidParams => -32602,
            Self::Internal => -32603,
            Self::NotInitialized => -32000,
            Self::AlreadyInitialized => -32001,
            Self::UpdateRequired => -32002,
            Self::CapabilityDenied => -32003,
            Self::NotFound => -32004,
            Self::VersionConflict => -32005,
            Self::InvalidValue => -32006,
            Self::Busy => -32007,
            Self::PortInUse => -32008,
            Self::ConfirmationRequired => -32009,
        }
    }

    pub const ALL: [Self; 15] = [
        Self::ParseError,
        Self::InvalidRequest,
        Self::MethodNotFound,
        Self::InvalidParams,
        Self::Internal,
        Self::NotInitialized,
        Self::AlreadyInitialized,
        Self::UpdateRequired,
        Self::CapabilityDenied,
        Self::NotFound,
        Self::VersionConflict,
        Self::InvalidValue,
        Self::Busy,
        Self::PortInUse,
        Self::ConfirmationRequired,
    ];
}

/// Machine-readable detail on an error.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ErrorData {
    pub kind: ErrorKind,
    /// For `update-required`: the oldest client version the daemon accepts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_client_version: Option<String>,
    /// For `version-conflict`: the entity's current version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::safe::optional_integer")]
    pub current_version: Option<u64>,
}

/// The JSON-RPC error object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiError {
    pub code: i32,
    pub message: String,
    pub data: ErrorData,
}

impl ApiError {
    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            code: kind.code(),
            message: message.into(),
            data: ErrorData {
                kind,
                min_client_version: None,
                current_version: None,
            },
        }
    }

    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        self.data.kind
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_unique() {
        let mut codes: Vec<i32> = ErrorKind::ALL.iter().map(|kind| kind.code()).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), ErrorKind::ALL.len());
    }
}

//! Conversions between `uxduck-core` and the wire DTOs; the only place both
//! are imported.

pub mod checklist;
mod client;
mod command;
mod controller;
mod files;
mod find;
mod history;
mod marker;
mod operator;
mod param_definitions;
mod parameter;
mod place;
mod plan;
mod plugin;
mod practice;
pub mod preflight;
pub mod settings;
mod setup;
mod snapshot;
mod telemetry;
mod thread;
mod vehicle;
mod video;

use uxduck_core::domain::client as core;

use crate::dto::initialize as dto;

impl From<dto::ClientKind> for core::ClientKind {
    fn from(kind: dto::ClientKind) -> Self {
        match kind {
            dto::ClientKind::OperatorUi => Self::OperatorUi,
            dto::ClientKind::OperatorCli => Self::OperatorCli,
            dto::ClientKind::Shell => Self::Shell,
            dto::ClientKind::Agent => Self::Agent,
            dto::ClientKind::ThirdParty => Self::ThirdParty,
            dto::ClientKind::Exporter => Self::Exporter,
            dto::ClientKind::RemoteOperator => Self::RemoteOperator,
            dto::ClientKind::Plugin => Self::Plugin,
            dto::ClientKind::NetworkUi => Self::NetworkUi,
        }
    }
}

impl From<core::ClientKind> for dto::ClientKind {
    fn from(kind: core::ClientKind) -> Self {
        match kind {
            core::ClientKind::OperatorUi => Self::OperatorUi,
            core::ClientKind::OperatorCli => Self::OperatorCli,
            core::ClientKind::Shell => Self::Shell,
            core::ClientKind::Agent => Self::Agent,
            core::ClientKind::ThirdParty => Self::ThirdParty,
            core::ClientKind::Exporter => Self::Exporter,
            core::ClientKind::RemoteOperator => Self::RemoteOperator,
            core::ClientKind::Plugin => Self::Plugin,
            core::ClientKind::NetworkUi => Self::NetworkUi,
        }
    }
}

impl From<crate::dto::pairing::NetworkAccess> for core::NetworkAccess {
    fn from(access: crate::dto::pairing::NetworkAccess) -> Self {
        match access {
            crate::dto::pairing::NetworkAccess::Watch => Self::Watch,
            crate::dto::pairing::NetworkAccess::Fly => Self::Fly,
        }
    }
}

impl From<core::NetworkAccess> for crate::dto::pairing::NetworkAccess {
    fn from(access: core::NetworkAccess) -> Self {
        match access {
            core::NetworkAccess::Watch => Self::Watch,
            core::NetworkAccess::Fly => Self::Fly,
        }
    }
}

impl From<core::Capability> for dto::Capability {
    fn from(capability: core::Capability) -> Self {
        match capability {
            core::Capability::Read => Self::Read,
            core::Capability::Control => Self::Control,
            core::Capability::Config => Self::Config,
            core::Capability::Approve => Self::Approve,
            core::Capability::Location => Self::Location,
        }
    }
}

impl From<dto::Capability> for core::Capability {
    fn from(capability: dto::Capability) -> Self {
        match capability {
            dto::Capability::Read => Self::Read,
            dto::Capability::Control => Self::Control,
            dto::Capability::Config => Self::Config,
            dto::Capability::Approve => Self::Approve,
            dto::Capability::Location => Self::Location,
        }
    }
}

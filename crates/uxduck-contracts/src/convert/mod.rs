//! Conversions between `uxduck-core` and the wire DTOs; the only place both
//! are imported.

mod client;
mod command;
mod controller;
mod files;
mod find;
mod history;
mod operator;
mod param_definitions;
mod parameter;
mod place;
mod plugin;
mod practice;
pub mod settings;
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
        }
    }
}

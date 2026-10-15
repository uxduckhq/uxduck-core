//! Clients: any admitted connection, with a kind and a capability set (ADR-0005).

use serde::{Deserialize, Serialize};

/// What a client declares itself to be in `initialize`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClientKind {
    OperatorUi,
    OperatorCli,
    Shell,
    Agent,
    ThirdParty,
    Exporter,
    RemoteOperator,
    /// A plugin the daemon runs (ADR-0006), never a socket client.
    Plugin,
}

impl ClientKind {
    /// Operator kinds have full control on admission; every other kind
    /// escalates on first need.
    #[must_use]
    pub fn is_operator(self) -> bool {
        matches!(self, Self::OperatorUi | Self::OperatorCli | Self::Shell)
    }
}

/// What a client may do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    Read,
    Control,
    Config,
    Approve,
}

impl Capability {
    pub const ALL: [Self; 4] = [Self::Read, Self::Control, Self::Config, Self::Approve];

    /// The error a connection gets when it lacks this capability, in the
    /// words the app shows.
    #[must_use]
    pub fn denied_message(self) -> String {
        let verb = match self {
            Self::Read => "read data from UXDuck",
            Self::Control => "control vehicles",
            Self::Config => "change settings",
            Self::Approve => "answer requests from other connections",
        };
        format!("This connection isn't allowed to {verb}.")
    }
}

/// A client identity and what it may do, remembered across connections
/// (ADR-0005). The id is derived from the identity, so the same agent always
/// finds its record: kind, declared name, and the peer's executable where
/// the transport shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientRecord {
    pub id: String,
    pub name: String,
    pub kind: ClientKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    /// Operators always hold every capability; this is for the rest.
    pub capabilities: Vec<Capability>,
    pub created_at_us: u64,
    pub version: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_operator_kinds_are_operators() {
        assert!(ClientKind::OperatorUi.is_operator());
        assert!(ClientKind::Shell.is_operator());
        assert!(!ClientKind::Agent.is_operator());
        assert!(!ClientKind::RemoteOperator.is_operator());
    }
}

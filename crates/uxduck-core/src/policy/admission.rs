//! What a connection is granted on `initialize` (ADR-0001 D5, ADR-0005).

use crate::domain::client::{Capability, ClientKind};

/// How the daemon knows who is on the other end of a connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trust {
    /// The local socket or named pipe, owned by the same user. Same-user
    /// processes are trusted to declare their kind honestly.
    LocalUser,
    /// A browser holding a session from a bootstrap token or an approved
    /// pairing.
    PairedBrowser { origin: String },
    /// A person away, through the account's relay (S80, ADR-0003 C9): one
    /// connection per cloud user, opened by the daemon's edge task once the
    /// cloud's signed grant naming them checks out.
    Relay { user_id: String, name: String },
}

/// Operators get full control on admission; everyone else starts with `read`
/// and escalates on first need (S10). A browser can only be an operator UI.
/// Someone away gets `read` and `control`; what they may send is then
/// decided by "Allow remote control from my account" (S80), never by an
/// approval for more.
#[must_use]
pub fn granted(kind: ClientKind, trust: &Trust) -> Vec<Capability> {
    let operator = match trust {
        Trust::LocalUser => kind.is_operator(),
        Trust::PairedBrowser { .. } => kind == ClientKind::OperatorUi,
        Trust::Relay { .. } if kind == ClientKind::RemoteOperator => {
            return vec![Capability::Read, Capability::Control];
        }
        Trust::Relay { .. } => false,
    };
    if operator {
        Capability::ALL.to_vec()
    } else {
        vec![Capability::Read]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn browser() -> Trust {
        Trust::PairedBrowser {
            origin: "http://localhost:5173".into(),
        }
    }

    #[test]
    fn operators_on_the_socket_get_everything() {
        for kind in [
            ClientKind::OperatorCli,
            ClientKind::Shell,
            ClientKind::OperatorUi,
        ] {
            assert_eq!(granted(kind, &Trust::LocalUser), Capability::ALL);
        }
    }

    #[test]
    fn agents_and_third_parties_start_with_read() {
        for kind in [
            ClientKind::Agent,
            ClientKind::ThirdParty,
            ClientKind::Exporter,
            ClientKind::RemoteOperator,
        ] {
            assert_eq!(granted(kind, &Trust::LocalUser), [Capability::Read]);
        }
    }

    #[test]
    fn a_browser_cannot_claim_to_be_the_cli_or_the_shell() {
        assert_eq!(granted(ClientKind::OperatorUi, &browser()), Capability::ALL);
        assert_eq!(
            granted(ClientKind::OperatorCli, &browser()),
            [Capability::Read]
        );
        assert_eq!(granted(ClientKind::Shell, &browser()), [Capability::Read]);
    }
}

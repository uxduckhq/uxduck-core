//! Browser sessions: one-time bootstrap tokens, pairing requests answered by an
//! operator, and the sessions both of them produce (ADR-0001, Transports and
//! authentication). Sessions survive restarts through the `Persistence` port,
//! stored by the SHA-256 of their token; bootstrap tokens and pairing
//! requests are short-lived and stay in memory.

use std::collections::HashMap;

use sha2::{Digest, Sha256};
use uxduck_contracts::dto::pairing::PairingRequest;

use crate::commands::Raise;
use crate::runtime::{Clock, Persistence, Random};

use uxduck_core::domain::client::NetworkAccess;
use uxduck_core::domain::notification::{
    Action, NotificationAction, NotificationKind, Severity, Subject,
};

const BOOTSTRAP_TTL_US: u64 = 60 * 1_000_000;
const PAIRING_TTL_US: u64 = 10 * 60 * 1_000_000;
/// A browser polls every second while it waits; one silent this long has
/// gone (closed, reloaded), and its question with it.
const ABANDONED_US: u64 = 15 * 1_000_000;
/// Phones and tablets on the network waiting at once (S97): anyone there
/// can open the page, so they cannot pile questions on the operator.
const NETWORK_WAITING: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub origin: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PairingState {
    Pending {
        code: String,
    },
    /// The browser collects its session once; the request is then removed.
    Approved {
        session: String,
        /// For a phone or tablet on the network (S97): what it was allowed.
        network: Option<NetworkPairing>,
    },
    Denied,
    /// Unknown or expired.
    Unknown,
}

/// A phone or tablet on the network that was allowed in (S97).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkPairing {
    pub access: NetworkAccess,
    /// What it is, from its browser: "iPhone", "Android phone".
    pub device: String,
    /// Its address on the network when it paired.
    pub peer: Option<String>,
}

/// The operator's answer: no, or yes with what a phone on the network may do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    Deny,
    Allow(NetworkAccess),
}

#[derive(Debug)]
struct Pending {
    code: String,
    origin: String,
    user_agent: Option<String>,
    /// Its address, when it asks through the network listener (S97).
    peer: Option<String>,
    requested_at_us: u64,
    polled_at_us: u64,
    answer: Option<Answer>,
}

/// A question to show while a browser waits.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Question {
    pub origin: String,
    pub code: String,
    pub device: Option<String>,
    pub peer: Option<String>,
}

impl Pending {
    /// Tabs of one browser on one site are asked as one.
    fn same_browser(&self, other: &Self) -> bool {
        self.origin == other.origin && self.user_agent == other.user_agent
    }
}

/// The client record of a phone or tablet paired on the network (S97): one
/// per session, so each device keeps what it was allowed.
#[must_use]
pub fn network_client(token: &str) -> String {
    client_of_hash(&token_hash(token))
}

fn client_of_hash(hash: &str) -> String {
    format!("clnet{}", &hash[..16.min(hash.len())])
}

/// Whether a session's origin is a page on the network listener (S97): an
/// IPv4 address that is not this computer's loopback.
#[must_use]
pub fn network_origin(origin: &str) -> bool {
    origin
        .strip_prefix("http://")
        .and_then(|host| host.rsplit_once(':').map(|(ip, _)| ip))
        .and_then(|ip| ip.parse::<std::net::Ipv4Addr>().ok())
        .is_some_and(|ip| !ip.is_loopback() && !ip.is_unspecified())
}

/// What a device is, from its browser's user agent, in a few words.
#[must_use]
pub fn device_name(user_agent: Option<&str>) -> String {
    let agent = user_agent.unwrap_or_default();
    let name = if agent.contains("iPhone") {
        "iPhone"
    } else if agent.contains("iPad") {
        "iPad"
    } else if agent.contains("Android") && agent.contains("Mobile") {
        "Android phone"
    } else if agent.contains("Android") {
        "Android tablet"
    } else if agent.contains("Macintosh") {
        "Mac"
    } else if agent.contains("Windows") {
        "Windows computer"
    } else if agent.contains("Linux") {
        "Linux computer"
    } else {
        "A device"
    };
    name.to_owned()
}

/// Lowercase hex SHA-256 of a session token: what is stored and looked up.
#[must_use]
pub fn token_hash(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            use std::fmt::Write;
            let _ = write!(out, "{byte:02x}");
            out
        })
}

#[derive(Debug, Default)]
pub struct Sessions {
    bootstrap: HashMap<String, (String, u64)>,
    pairing: HashMap<String, Pending>,
    /// By token hash.
    active: HashMap<String, Session>,
}

impl Sessions {
    /// Sessions saved before a restart, by token hash.
    #[must_use]
    pub fn restored(sessions: impl IntoIterator<Item = (String, Session)>) -> Self {
        Self {
            active: sessions.into_iter().collect(),
            ..Self::default()
        }
    }

    /// A token that `exchange_bootstrap` turns into a session once, within a minute.
    pub fn create_bootstrap(
        &mut self,
        origin: &str,
        clock: &dyn Clock,
        random: &mut dyn Random,
    ) -> String {
        let token = random.token();
        self.bootstrap
            .insert(token.clone(), (origin.to_owned(), clock.now_us()));
        token
    }

    pub fn exchange_bootstrap(
        &mut self,
        token: &str,
        clock: &dyn Clock,
        random: &mut dyn Random,
        store: &mut dyn Persistence,
    ) -> Option<String> {
        let (origin, created) = self.bootstrap.remove(token)?;
        if clock.now_us().saturating_sub(created) > BOOTSTRAP_TTL_US {
            return None;
        }
        Some(self.create_session(origin, random, store))
    }

    /// Starts a pairing request. Returns its id, which only the browser
    /// knows, and the code it shows. A phone or tablet on the network gives
    /// its `peer` address; none when too many of those wait already.
    pub fn request_pairing(
        &mut self,
        origin: &str,
        user_agent: Option<String>,
        peer: Option<String>,
        clock: &dyn Clock,
        random: &mut dyn Random,
    ) -> Option<(String, String)> {
        self.expire(clock);
        let waiting = self
            .pairing
            .values()
            .filter(|pending| pending.peer.is_some() && pending.answer.is_none())
            .count();
        if peer.is_some() && waiting >= NETWORK_WAITING {
            return None;
        }
        let code = loop {
            let code = random.code();
            if !self.pairing.values().any(|pending| pending.code == code) {
                break code;
            }
        };
        let id = random.token();
        self.pairing.insert(
            id.clone(),
            Pending {
                code: code.clone(),
                origin: origin.to_owned(),
                user_agent,
                peer,
                requested_at_us: clock.now_us(),
                polled_at_us: clock.now_us(),
                answer: None,
            },
        );
        Some((id, code))
    }

    pub fn poll_pairing(
        &mut self,
        id: &str,
        clock: &dyn Clock,
        random: &mut dyn Random,
        store: &mut dyn Persistence,
    ) -> PairingState {
        self.expire(clock);
        let Some(pending) = self.pairing.get_mut(id) else {
            return PairingState::Unknown;
        };
        pending.polled_at_us = clock.now_us();
        match pending.answer {
            None => PairingState::Pending {
                code: pending.code.clone(),
            },
            Some(Answer::Deny) => {
                self.pairing.remove(id);
                PairingState::Denied
            }
            Some(Answer::Allow(access)) => {
                let origin = pending.origin.clone();
                let network = pending.peer.as_ref().map(|peer| NetworkPairing {
                    access,
                    device: device_name(pending.user_agent.as_deref()),
                    peer: Some(peer.clone()),
                });
                self.pairing.remove(id);
                PairingState::Approved {
                    session: self.create_session(origin, random, store),
                    network,
                }
            }
        }
    }

    /// Records the operator's answer, for every tab of the same browser on
    /// the same site that waits too. False if no pending request shows `code`.
    /// `access` is what a phone or tablet on the network may do: watching
    /// unless said otherwise.
    pub fn answer_pairing(
        &mut self,
        code: &str,
        approve: bool,
        access: Option<NetworkAccess>,
        clock: &dyn Clock,
    ) -> bool {
        self.expire(clock);
        let Some(asked) = self
            .pairing
            .values()
            .find(|pending| pending.code == code && pending.answer.is_none())
            .map(|pending| (pending.origin.clone(), pending.user_agent.clone()))
        else {
            return false;
        };
        for pending in self.pairing.values_mut() {
            if pending.answer.is_none()
                && pending.origin == asked.0
                && pending.user_agent == asked.1
            {
                pending.answer = Some(if approve {
                    Answer::Allow(access.unwrap_or_default())
                } else {
                    Answer::Deny
                });
            }
        }
        true
    }

    /// The questions to show: one per browser and site still waiting, with
    /// the code of its newest tab, the one most likely in front of the
    /// person. Expired and abandoned requests are dropped first.
    pub(crate) fn questions(&mut self, clock: &dyn Clock) -> Vec<Question> {
        self.expire(clock);
        let waiting: Vec<&Pending> = self
            .pairing
            .values()
            .filter(|pending| pending.answer.is_none())
            .collect();
        let mut questions: Vec<Question> = waiting
            .iter()
            .filter(|pending| {
                !waiting.iter().any(|other| {
                    other.same_browser(pending)
                        && (other.requested_at_us, &other.code)
                            > (pending.requested_at_us, &pending.code)
                })
            })
            .map(|pending| Question {
                origin: pending.origin.clone(),
                code: pending.code.clone(),
                device: pending
                    .peer
                    .as_ref()
                    .map(|_| device_name(pending.user_agent.as_deref())),
                peer: pending.peer.clone(),
            })
            .collect();
        questions.sort();
        questions
    }

    pub fn pending_pairings(&mut self, clock: &dyn Clock) -> Vec<PairingRequest> {
        self.expire(clock);
        let mut requests: Vec<PairingRequest> = self
            .pairing
            .values()
            .filter(|pending| pending.answer.is_none())
            .map(|pending| PairingRequest {
                code: pending.code.clone(),
                origin: pending.origin.clone(),
                user_agent: pending.user_agent.clone(),
                network_peer: pending.peer.clone(),
                requested_at_us: pending.requested_at_us,
            })
            .collect();
        requests.sort_by_key(|request| request.requested_at_us);
        requests
    }

    /// Whether any browser has paired with this install.
    #[must_use]
    pub fn has_sessions(&self) -> bool {
        !self.active.is_empty()
    }

    #[must_use]
    pub fn session(&self, token: &str) -> Option<&Session> {
        self.active.get(&token_hash(token))
    }

    /// Forgets sessions of phones and tablets whose client record is gone
    /// (S97), as when one was forgotten just before a crash.
    pub fn forget_orphaned_network_sessions(
        &mut self,
        known: impl Fn(&str) -> bool,
        store: &mut dyn Persistence,
    ) {
        let gone: Vec<String> = self
            .active
            .iter()
            .filter(|(hash, session)| {
                network_origin(&session.origin) && !known(&client_of_hash(hash))
            })
            .map(|(hash, _)| hash.clone())
            .collect();
        for hash in gone {
            self.active.remove(&hash);
            store.delete_session(&hash);
        }
    }

    /// Forgets the sessions of a phone or tablet paired on the network
    /// (S97): its cookie no longer counts, so it must pair again.
    pub fn forget_network_client(&mut self, client: &str, store: &mut dyn Persistence) {
        let gone: Vec<String> = self
            .active
            .keys()
            .filter(|hash| client_of_hash(hash) == client)
            .cloned()
            .collect();
        for hash in gone {
            self.active.remove(&hash);
            store.delete_session(&hash);
        }
    }

    fn create_session(
        &mut self,
        origin: String,
        random: &mut dyn Random,
        store: &mut dyn Persistence,
    ) -> String {
        let token = random.token();
        let hash = token_hash(&token);
        let session = Session { origin };
        store.save_session(&hash, &session);
        self.active.insert(hash, session);
        token
    }

    fn expire(&mut self, clock: &dyn Clock) {
        let now = clock.now_us();
        self.pairing.retain(|_, pending| {
            now.saturating_sub(pending.requested_at_us) <= PAIRING_TTL_US
                && (pending.answer.is_some()
                    || now.saturating_sub(pending.polled_at_us) <= ABANDONED_US)
        });
        self.bootstrap
            .retain(|_, (_, created)| now.saturating_sub(*created) <= BOOTSTRAP_TTL_US);
    }
}

/// What a pairing question is bound to, resolved once it is answered.
#[must_use]
pub fn condition(code: &str) -> String {
    format!("pairing:{code}")
}

/// The question every operator's app shows while a browser waits: who asks,
/// its code to compare with the one on its page, and Allow or Deny. A phone
/// or tablet on the network is named with its address, and is allowed to
/// watch, or to watch and fly (S97).
pub(crate) fn question(asked: &Question) -> Raise {
    let Question {
        origin,
        code,
        device,
        peer,
    } = asked;
    let answer = |label: &str, allow, access| NotificationAction {
        label: label.into(),
        action: Action::AnswerPairing {
            code: code.clone(),
            allow,
            access,
        },
    };
    let spaced = format!(
        "{} {}",
        &code[..code.len().min(3)],
        &code[code.len().min(3)..]
    );
    if let (Some(device), Some(peer)) = (device, peer) {
        return Raise {
            severity: Severity::Warning,
            kind: NotificationKind::Persistent,
            subject: Subject::System,
            title: format!("{device} at {peer} asks to connect to UXDuck"),
            body: Some(format!(
                "From this computer's network. Allow it only if it's your device and it shows {spaced}. Watching can't send commands. Flying can."
            )),
            actions: vec![
                answer("Watch only", true, Some(NetworkAccess::Watch)),
                answer("Watch and fly", true, Some(NetworkAccess::Fly)),
                answer("Deny", false, None),
            ],
            condition: Some(condition(code)),
            report: None,
        };
    }
    let site = origin
        .split_once("://")
        .map_or(origin.as_str(), |(_, host)| host)
        .trim_end_matches('/');
    let site = if site.is_empty() { "A browser" } else { site };
    Raise {
        severity: Severity::Warning,
        kind: NotificationKind::Persistent,
        subject: Subject::System,
        title: format!("{site} asks to connect to UXDuck"),
        body: Some(format!(
            "Allow it only if you asked to connect just now and the page shows {spaced}."
        )),
        actions: vec![answer("Allow", true, None), answer("Deny", false, None)],
        condition: Some(condition(code)),
        report: None,
    }
}

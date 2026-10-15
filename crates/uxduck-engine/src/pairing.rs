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

use uxduck_core::domain::notification::{
    Action, NotificationAction, NotificationKind, Severity, Subject,
};

const BOOTSTRAP_TTL_US: u64 = 60 * 1_000_000;
const PAIRING_TTL_US: u64 = 10 * 60 * 1_000_000;
/// A browser polls every second while it waits; one silent this long has
/// gone (closed, reloaded), and its question with it.
const ABANDONED_US: u64 = 15 * 1_000_000;

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
    },
    Denied,
    /// Unknown or expired.
    Unknown,
}

#[derive(Debug)]
struct Pending {
    code: String,
    origin: String,
    user_agent: Option<String>,
    requested_at_us: u64,
    polled_at_us: u64,
    answer: Option<bool>,
}

impl Pending {
    /// Tabs of one browser on one site are asked as one.
    fn same_browser(&self, other: &Self) -> bool {
        self.origin == other.origin && self.user_agent == other.user_agent
    }
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
    /// knows, and the code it shows.
    pub fn request_pairing(
        &mut self,
        origin: &str,
        user_agent: Option<String>,
        clock: &dyn Clock,
        random: &mut dyn Random,
    ) -> (String, String) {
        self.expire(clock);
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
                requested_at_us: clock.now_us(),
                polled_at_us: clock.now_us(),
                answer: None,
            },
        );
        (id, code)
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
            Some(false) => {
                self.pairing.remove(id);
                PairingState::Denied
            }
            Some(true) => {
                let origin = pending.origin.clone();
                self.pairing.remove(id);
                PairingState::Approved {
                    session: self.create_session(origin, random, store),
                }
            }
        }
    }

    /// Records the operator's answer, for every tab of the same browser on
    /// the same site that waits too. False if no pending request shows `code`.
    pub fn answer_pairing(&mut self, code: &str, approve: bool, clock: &dyn Clock) -> bool {
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
                pending.answer = Some(approve);
            }
        }
        true
    }

    /// The questions to show: one per browser and site still waiting, with
    /// the code of its newest tab, the one most likely in front of the
    /// person. Expired and abandoned requests are dropped first.
    pub fn questions(&mut self, clock: &dyn Clock) -> Vec<(String, String)> {
        self.expire(clock);
        let waiting: Vec<&Pending> = self
            .pairing
            .values()
            .filter(|pending| pending.answer.is_none())
            .collect();
        let mut questions: Vec<(String, String)> = waiting
            .iter()
            .filter(|pending| {
                !waiting.iter().any(|other| {
                    other.same_browser(pending)
                        && (other.requested_at_us, &other.code)
                            > (pending.requested_at_us, &pending.code)
                })
            })
            .map(|pending| (pending.origin.clone(), pending.code.clone()))
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
/// its code to compare with the one on its page, and Allow or Deny.
pub(crate) fn question(origin: &str, code: &str) -> Raise {
    let site = origin
        .split_once("://")
        .map_or(origin, |(_, host)| host)
        .trim_end_matches('/');
    let site = if site.is_empty() { "A browser" } else { site };
    let answer = |label: &str, allow| NotificationAction {
        label: label.into(),
        action: Action::AnswerPairing {
            code: code.to_owned(),
            allow,
        },
    };
    Raise {
        severity: Severity::Warning,
        kind: NotificationKind::Persistent,
        subject: Subject::System,
        title: format!("{site} asks to connect to UXDuck"),
        body: Some(format!(
            "Allow it only if you asked to connect just now and the page shows {} {}.",
            &code[..code.len().min(3)],
            &code[code.len().min(3)..]
        )),
        actions: vec![answer("Allow", true), answer("Deny", false)],
        condition: Some(condition(code)),
        report: None,
    }
}

//! Cloud sync in the browser (S16, ADR-0011): the installed daemon's rounds
//! (`crates/uxduck/src/sync.rs`) as steps the worker drives with its own
//! fetches, which carry this browser's session cookie. Events are read for
//! the account as the journal takes them; after a visit, those the journal
//! kept since its checkpoint are read again, and the whole of this
//! browser's work when the checkpoint is past what the account has.
//!
//! A round sends what is waiting, then takes what other computers and
//! browsers sent, every [`EVERY_US`] and soon after a change.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::json;
use uxduck_core::events::Event;
use uxduck_engine::Engine;
use uxduck_engine::sync::{Outgoing, Pulled, SyncChange, SyncPort, outgoing};

use crate::root::OFFLINE;

/// What this browser is called in the account.
pub const DEVICE_NAME: &str = "UXDuck Web";
/// How long after a change a round starts, so a burst goes together.
pub const SETTLE_US: u64 = 300_000;
/// How often other computers' changes are fetched.
pub const EVERY_US: u64 = 30_000_000;
/// Changes per push, and the most bytes of them, as the daemon sends.
const BATCH: usize = 200;
const BATCH_BYTES: usize = 2 * 1024 * 1024;

/// A request for the worker to make at `/v1/sync/{path}` and answer with
/// [`crate::root::Browser::synced`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub request: u32,
    pub method: &'static str,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// The engine's end: a nudge that something is waiting to be sent.
#[derive(Debug)]
pub struct Wake(pub Arc<AtomicBool>);

impl SyncPort for Wake {
    fn wake(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// The latest change of each entity waiting for the account.
#[derive(Debug, Default)]
pub struct Outbox {
    scanned_through: u64,
    waiting: BTreeMap<(String, String), SyncChange>,
    /// The digest last sent for each entity, so the same copy is not sent
    /// again.
    sent: HashMap<(String, String), String>,
}

impl Outbox {
    /// Reads one event the journal took.
    pub fn read(&mut self, event: &Event) {
        self.scanned_through = self.scanned_through.max(event.seq);
        match outgoing(event) {
            Outgoing::Send(change) => {
                let key = (change.entity.clone(), change.id.clone());
                if self.sent.get(&key) == Some(&change.digest) {
                    self.waiting.remove(&key);
                } else {
                    self.waiting.insert(key, *change);
                }
            }
            Outgoing::Withdraw { entity, id } => {
                self.waiting.remove(&(entity, id));
            }
            Outgoing::Ignore => {}
        }
    }

    /// The account has these; what changed since stays waiting.
    fn taken(&mut self, changes: &[SyncChange]) {
        for change in changes {
            let key = (change.entity.clone(), change.id.clone());
            if self
                .waiting
                .get(&key)
                .is_some_and(|waiting| waiting.digest == change.digest)
            {
                self.waiting.remove(&key);
            }
            self.sent.insert(key, change.digest.clone());
        }
    }
}

pub type SharedOutbox = Arc<Mutex<Outbox>>;

fn lock(outbox: &SharedOutbox) -> std::sync::MutexGuard<'_, Outbox> {
    outbox.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug)]
enum Step {
    Push {
        through: u64,
        sending: Vec<SyncChange>,
        rest: VecDeque<Vec<SyncChange>>,
        sent: u64,
    },
    Pull {
        after: u64,
    },
}

#[derive(Deserialize)]
struct PullPage {
    changes: Vec<Pulled>,
    cursor: u64,
    more: bool,
}

/// One browser's rounds.
#[derive(Debug)]
pub struct Syncer {
    pub outbox: SharedOutbox,
    pub woken: Arc<AtomicBool>,
    woken_at: Option<u64>,
    next_round_us: u64,
    step: Option<(u32, Step)>,
    next_request: u32,
}

impl Syncer {
    #[must_use]
    pub fn new(outbox: SharedOutbox) -> Self {
        Self {
            outbox,
            woken: Arc::default(),
            woken_at: None,
            next_round_us: 0,
            step: None,
            next_request: 0,
        }
    }

    /// Starts a round at the next poll, as after signing in.
    pub fn soon(&mut self) {
        self.next_round_us = 0;
    }

    /// Starts a round when one is due: the request to make, if any.
    pub fn poll(
        &mut self,
        engine: &mut Engine,
        account: Option<&str>,
        now: u64,
    ) -> Option<Request> {
        if self.woken.swap(false, Ordering::SeqCst) {
            self.woken_at.get_or_insert(now);
        }
        let state = engine.sync_state().clone();
        if self.step.is_some() || !state.enabled {
            return None;
        }
        let settled = self.woken_at.is_some_and(|at| now >= at + SETTLE_US);
        if !settled && now < self.next_round_us {
            return None;
        }
        self.woken_at = None;
        self.next_round_us = now + EVERY_US;
        let Some(account) = account else {
            engine.sync_problem(Some("Sign in to sync.".into()));
            return None;
        };
        if let Err(problem) = engine.sync_account(account) {
            engine.sync_problem(Some(problem));
            return None;
        }
        let state = engine.sync_round();
        let (through, waiting) = {
            let outbox = lock(&self.outbox);
            let waiting: Vec<SyncChange> = outbox
                .waiting
                .values()
                .filter(|change| state.syncs(&change.entity))
                .cloned()
                .collect();
            (outbox.scanned_through.max(state.exported_through), waiting)
        };
        let mut rest: VecDeque<Vec<SyncChange>> = batches(waiting).into();
        let Some(sending) = rest.pop_front() else {
            engine.sync_scanned(through);
            return Some(self.pull(engine, state.pulled_through));
        };
        Some(self.push(&state.device_id, through, sending, rest, 0))
    }

    /// The worker's answer to a request: the next request of the round, if
    /// any. `status` is the HTTP status, or 0 with `error` when there was
    /// none.
    pub fn answer(
        &mut self,
        engine: &mut Engine,
        account: Option<&str>,
        request: u32,
        status: u16,
        body: Option<&str>,
        error: Option<&str>,
    ) -> Option<Request> {
        let (expected, step) = self.step.take()?;
        if expected != request {
            self.step = Some((expected, step));
            return None;
        }
        if let Some(problem) = refused(status, error) {
            engine.sync_problem(Some(problem));
            return None;
        }
        if account != Some(engine.sync_state().account_id.as_str()) {
            engine.sync_problem(Some("The account changed. Sync is paused.".into()));
            return None;
        }
        match step {
            Step::Push {
                through,
                sending,
                mut rest,
                sent,
            } => {
                lock(&self.outbox).taken(&sending);
                let sent = sent + sending.len() as u64;
                if let Some(next) = rest.pop_front() {
                    let device = engine.sync_state().device_id.clone();
                    return Some(self.push(&device, through, next, rest, sent));
                }
                engine.sync_exported(through, sent);
                let after = engine.sync_state().pulled_through;
                Some(self.pull(engine, after))
            }
            Step::Pull { after } => {
                let Some(page) = body.and_then(|body| serde_json::from_str::<PullPage>(body).ok())
                else {
                    engine.sync_problem(Some(
                        "The account server's answer was not understood.".into(),
                    ));
                    return None;
                };
                let (cursor, more) = (page.cursor, page.more);
                engine.sync_pulled(page.changes, cursor);
                if more && cursor > after {
                    return Some(self.pull(engine, cursor));
                }
                engine.sync_problem(None);
                None
            }
        }
    }

    fn push(
        &mut self,
        device: &str,
        through: u64,
        sending: Vec<SyncChange>,
        rest: VecDeque<Vec<SyncChange>>,
        sent: u64,
    ) -> Request {
        let body = json!({
            "deviceId": device,
            "deviceName": DEVICE_NAME,
            "changes": sending,
        })
        .to_string();
        self.request(
            "POST",
            "push".into(),
            Some(body),
            Step::Push {
                through,
                sending,
                rest,
                sent,
            },
        )
    }

    fn pull(&mut self, engine: &Engine, after: u64) -> Request {
        let path = format!(
            "pull?device={}&after={after}",
            engine.sync_state().device_id
        );
        self.request("GET", path, None, Step::Pull { after })
    }

    fn request(
        &mut self,
        method: &'static str,
        path: String,
        body: Option<String>,
        step: Step,
    ) -> Request {
        self.next_request += 1;
        self.step = Some((self.next_request, step));
        Request {
            request: self.next_request,
            method,
            path,
            body,
        }
    }
}

/// Why an answer stops the round, said as the daemon says it.
fn refused(status: u16, error: Option<&str>) -> Option<String> {
    if let Some(error) = error {
        return Some(if error.starts_with(OFFLINE) {
            "UXDuck cannot reach its server. It will try again.".into()
        } else {
            error.to_owned()
        });
    }
    match status {
        200..=299 => None,
        401 => Some("Sign in again to sync.".into()),
        503 => Some("Sync is not set up on the account server yet.".into()),
        status => Some(format!("The account server said {status}.")),
    }
}

/// Changes in pushes of at most [`BATCH`] changes and about [`BATCH_BYTES`].
fn batches(changes: Vec<SyncChange>) -> Vec<Vec<SyncChange>> {
    let mut out: Vec<Vec<SyncChange>> = Vec::new();
    let mut bytes = 0;
    for change in changes {
        let size = serde_json::to_vec(&change).map_or(0, |v| v.len());
        let full = out
            .last()
            .is_none_or(|batch| batch.len() >= BATCH || bytes + size > BATCH_BYTES);
        if full {
            out.push(Vec::new());
            bytes = 0;
        }
        bytes += size;
        if let Some(batch) = out.last_mut() {
            batch.push(change);
        }
    }
    out
}

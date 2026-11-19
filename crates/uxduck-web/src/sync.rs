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
use uxduck_engine::sync::{
    Outgoing, Pulled, PushAnswer, SyncChange, SyncPort, outgoing, refused_alone,
};

use crate::root::OFFLINE;

/// What this browser is called in the account.
pub const DEVICE_NAME: &str = "UXDuck Web";
/// How long after a change a round starts, so a burst goes together.
pub const SETTLE_US: u64 = 300_000;
/// How often other computers' changes are fetched.
pub const EVERY_US: u64 = 30_000_000;
/// Changes per push, and about the most bytes of them, as the daemon sends
/// and for its reason: a larger change goes by itself, and the account
/// server says whether it takes it.
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
        /// These were refused before and are sent again, once a round.
        again: bool,
    },
    Pull {
        after: u64,
    },
}

#[derive(Deserialize)]
struct PullPage {
    /// Read one by one, so a kind of change a newer UXDuck sends is passed
    /// over instead of stopping every pull (FB-229).
    changes: Vec<serde_json::Value>,
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
        // A browser has no cameras of its own, so it asks for none (FB-229).
        let state = engine.sync_round(&[]);
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
            // Something waits for room in the account: a push of nothing
            // asks what the account holds now.
            if state.waits_for_room() {
                return Some(self.push(&state.device_id, through, Vec::new(), rest, 0, false));
            }
            return Some(self.pull(engine, state.pulled_through));
        };
        Some(self.push(&state.device_id, through, sending, rest, 0, false))
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
        // One change that is a whole push too large: the round carries on.
        let alone_too_large =
            status == 413 && matches!(&step, Step::Push { sending, .. } if sending.len() == 1);
        if !alone_too_large && let Some(problem) = refused(status, error) {
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
                again,
            } => {
                // What the account would not keep for its size, to tell the
                // person. It is not sent again until it changes, or until
                // the account can take it.
                let answer = PushAnswer::read(body.unwrap_or_default());
                let too_large = match sending.as_slice() {
                    [alone] if alone_too_large => {
                        vec![refused_alone(alone, body.unwrap_or_default())]
                    }
                    _ => answer.refused_of(&sending),
                };
                lock(&self.outbox).taken(&sending);
                let sent = sent + (sending.len() - too_large.len()) as u64;
                if !too_large.is_empty() || !engine.sync_state().too_large.is_empty() {
                    let kept: Vec<(String, String)> = sending
                        .iter()
                        .filter(|change| {
                            !too_large
                                .iter()
                                .any(|one| one.entity == change.entity && one.id == change.id)
                        })
                        .map(|change| (change.entity.clone(), change.id.clone()))
                        .collect();
                    engine.sync_sizes(&kept, too_large);
                }
                let device = engine.sync_state().device_id.clone();
                if let Some(next) = rest.pop_front() {
                    return Some(self.push(&device, through, next, rest, sent, again));
                }
                if !again {
                    engine.sync_exported(through, sent);
                    // What the account refused before and may take now.
                    let mut rest: VecDeque<Vec<SyncChange>> =
                        batches(engine.sync_again(&answer)).into();
                    if let Some(next) = rest.pop_front() {
                        return Some(self.push(&device, through, next, rest, 0, true));
                    }
                }
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
                let changes = page
                    .changes
                    .into_iter()
                    .filter_map(|change| serde_json::from_value::<Pulled>(change).ok())
                    .collect();
                engine.sync_pulled(changes, cursor);
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
        again: bool,
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
                again,
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

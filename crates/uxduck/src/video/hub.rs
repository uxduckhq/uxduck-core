//! Who watches which source, and each watcher's queue (ADR-0017 V6, V7).
//! A watcher starts at a keyframe; when its oldest queued picture is more
//! than 200 ms old the queue is dropped and it waits for the next keyframe,
//! marked as a discontinuity, so a slow client freezes briefly and catches
//! up rather than falling behind.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use uxduck_media::au::AccessUnit;

/// How much a watcher may fall behind (V7).
pub const MAX_QUEUED: Duration = Duration::from_millis(200);
/// A token not used within this long is forgotten.
const TOKEN_LIFETIME: Duration = Duration::from_secs(30);

/// One thing to send.
#[derive(Clone, Debug)]
pub enum Item {
    Unit(Arc<AccessUnit>),
    Klv { timestamp_us: u64, data: Arc<[u8]> },
}

#[derive(Debug, Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent states of one queue"
)]
struct Queue {
    items: VecDeque<(Instant, Item)>,
    /// Waiting for a keyframe: at the start, after a drop or when asked.
    skipping: bool,
    /// The next picture follows dropped ones.
    discontinuity: bool,
    started: bool,
    closed: bool,
    dropped: u64,
    sent: u64,
}

/// One `/video` socket's queue.
#[derive(Debug)]
pub struct Watcher {
    queue: Mutex<Queue>,
    ready: tokio::sync::Notify,
}

impl Default for Watcher {
    fn default() -> Self {
        Self {
            queue: Mutex::new(Queue {
                skipping: true,
                ..Queue::default()
            }),
            ready: tokio::sync::Notify::new(),
        }
    }
}

impl Watcher {
    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn push(&self, item: Item, now: Instant) {
        let mut q = self.lock();
        if q.closed {
            return;
        }
        if q.items
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) > MAX_QUEUED)
        {
            q.dropped += q.items.len() as u64;
            q.items.clear();
            q.skipping = true;
        }
        if let Item::Unit(unit) = &item {
            if q.skipping {
                if !unit.key {
                    q.dropped += 1;
                    return;
                }
                q.skipping = false;
                q.discontinuity = q.started;
                q.started = true;
            }
        } else if !q.started {
            // KLV waits for the first picture too.
            return;
        }
        q.items.push_back((now, item));
        drop(q);
        self.ready.notify_one();
    }

    /// Nothing but a keyframe next: the client's decoder failed (V8).
    pub fn wait_for_keyframe(&self) {
        let mut q = self.lock();
        q.items.retain(|(_, item)| !matches!(item, Item::Unit(_)));
        q.skipping = true;
    }

    pub fn close(&self) {
        self.lock().closed = true;
        self.ready.notify_one();
    }

    /// Pictures dropped for this watcher so far.
    #[cfg(test)]
    pub fn dropped(&self) -> u64 {
        self.lock().dropped
    }

    /// The next item and whether pictures were dropped before it; `None`
    /// once closed.
    pub async fn next(&self) -> Option<(Item, bool)> {
        loop {
            {
                let mut q = self.lock();
                if q.closed {
                    return None;
                }
                if let Some((_, item)) = q.items.pop_front() {
                    q.sent += 1;
                    let gap = matches!(item, Item::Unit(_)) && std::mem::take(&mut q.discontinuity);
                    return Some((item, gap));
                }
            }
            self.ready.notified().await;
        }
    }
}

#[derive(Debug, Default)]
struct Inner {
    /// Tokens the engine handed out, not yet opened.
    allowed: HashMap<String, (String, Instant)>,
    /// Open sockets by token: their source and queue.
    open: HashMap<String, (String, Arc<Watcher>)>,
    /// Recordings by id: their source, and their writer's end (S40).
    recorders: HashMap<String, (String, std::sync::mpsc::Sender<Item>)>,
    /// Playback tokens: the recording, and when the token was given.
    playback: HashMap<String, (String, Instant)>,
}

/// How long a playback token lasts.
const PLAYBACK_LIFETIME: Duration = Duration::from_secs(3600);

/// Shared by the listener, the `/video` route and the engine's port.
#[derive(Debug, Default)]
pub struct Hub {
    inner: Mutex<Inner>,
    /// Where recordings are kept: the Video storage category.
    dir: std::path::PathBuf,
}

impl Hub {
    #[must_use]
    pub fn new(dir: std::path::PathBuf) -> Self {
        Self {
            inner: Mutex::default(),
            dir,
        }
    }

    #[must_use]
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// Sends a source's pictures to a recording's writer too.
    pub fn record(&self, recording_id: &str, source_id: &str, to: std::sync::mpsc::Sender<Item>) {
        self.lock()
            .recorders
            .insert(recording_id.to_owned(), (source_id.to_owned(), to));
    }

    /// Ends a recording: its writer finishes the file.
    pub fn stop_recording(&self, recording_id: &str) {
        self.lock().recorders.remove(recording_id);
    }

    pub fn allow_playback(&self, token: &str, recording_id: &str) {
        let mut inner = self.lock();
        let now = Instant::now();
        inner
            .playback
            .retain(|_, (_, at)| now.duration_since(*at) < PLAYBACK_LIFETIME);
        inner
            .playback
            .insert(token.to_owned(), (recording_id.to_owned(), now));
    }

    /// The recording a playback token is for, while it lasts.
    pub fn playback(&self, token: &str) -> Option<String> {
        let inner = self.lock();
        let (id, at) = inner.playback.get(token)?;
        (at.elapsed() < PLAYBACK_LIFETIME).then(|| id.clone())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn allow(&self, token: &str, source_id: &str) {
        let mut inner = self.lock();
        let now = Instant::now();
        inner
            .allowed
            .retain(|_, (_, at)| now.duration_since(*at) < TOKEN_LIFETIME);
        inner
            .allowed
            .insert(token.to_owned(), (source_id.to_owned(), now));
    }

    pub fn revoke(&self, token: &str) {
        let mut inner = self.lock();
        inner.allowed.remove(token);
        if let Some((_, watcher)) = inner.open.remove(token) {
            watcher.close();
        }
    }

    /// A socket opens with a token: once, and within 30 s.
    pub fn open(&self, token: &str) -> Option<(String, Arc<Watcher>)> {
        let mut inner = self.lock();
        let (source_id, at) = inner.allowed.remove(token)?;
        if at.elapsed() >= TOKEN_LIFETIME {
            return None;
        }
        let watcher = Arc::new(Watcher::default());
        inner
            .open
            .insert(token.to_owned(), (source_id.clone(), watcher.clone()));
        Some((source_id, watcher))
    }

    /// A watcher of a source for this daemon itself, by `key`: the live
    /// publisher (S84). Like a socket, it falls to the next keyframe when
    /// it is 200 ms behind, so a slow uplink loses pictures, never latency.
    pub fn tap(&self, key: &str, source_id: &str) -> Arc<Watcher> {
        let watcher = Arc::new(Watcher::default());
        let old = self
            .lock()
            .open
            .insert(key.to_owned(), (source_id.to_owned(), watcher.clone()));
        if let Some((_, old)) = old {
            old.close();
        }
        watcher
    }

    pub fn close(&self, token: &str) {
        if let Some((_, watcher)) = self.lock().open.remove(token) {
            watcher.close();
        }
    }

    #[cfg(test)]
    pub fn watched(&self, source_id: &str) -> bool {
        self.lock().open.values().any(|(s, _)| s == source_id)
    }

    /// The open sockets: their source, what each was sent and dropped.
    pub fn sockets(&self) -> Vec<uxduck_contracts::dto::video::VideoSocket> {
        self.lock()
            .open
            .values()
            .map(|(source_id, w)| {
                let q = w.lock();
                uxduck_contracts::dto::video::VideoSocket {
                    source_id: source_id.clone(),
                    sent: q.sent,
                    dropped: q.dropped,
                }
            })
            .collect()
    }

    pub fn publish(&self, source_id: &str, item: &Item) {
        let now = Instant::now();
        let watchers: Vec<Arc<Watcher>> = self
            .lock()
            .open
            .values()
            .filter(|(s, _)| s == source_id)
            .map(|(_, w)| w.clone())
            .collect();
        for watcher in watchers {
            watcher.push(item.clone(), now);
        }
        // Recordings get every picture: no dropping to catch up.
        let mut inner = self.lock();
        inner
            .recorders
            .retain(|_, (source, to)| source != source_id || to.send(item.clone()).is_ok());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uxduck_media::nal::SpsInfo;

    fn unit(key: bool, timestamp_us: u64) -> Item {
        Item::Unit(Arc::new(AccessUnit {
            data: vec![0, 0, 0, 1],
            key,
            timestamp_us,
            sps: SpsInfo {
                id: 0,
                codec: "avc1.64001f".into(),
                coded_width: None,
                coded_height: None,
                display_width: None,
                display_height: None,
            },
        }))
    }

    fn drain(watcher: &Watcher) -> Vec<(u64, bool)> {
        let mut out = Vec::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        while !watcher.lock().items.is_empty() {
            if let Some((Item::Unit(u), gap)) = runtime.block_on(watcher.next()) {
                out.push((u.timestamp_us, gap));
            }
        }
        out
    }

    #[test]
    fn a_watcher_starts_at_a_keyframe() {
        let watcher = Watcher::default();
        let now = Instant::now();
        watcher.push(unit(false, 1), now);
        watcher.push(unit(true, 2), now);
        watcher.push(unit(false, 3), now);
        assert_eq!(drain(&watcher), [(2, false), (3, false)]);
    }

    #[test]
    fn falling_200_ms_behind_drops_to_the_next_keyframe_marked() {
        let watcher = Watcher::default();
        let start = Instant::now();
        watcher.push(unit(true, 1), start);
        watcher.push(unit(false, 2), start);
        let late = start + Duration::from_millis(250);
        watcher.push(unit(false, 3), late);
        watcher.push(unit(true, 4), late);
        watcher.push(unit(false, 5), late);
        assert_eq!(drain(&watcher), [(4, true), (5, false)]);
        assert_eq!(watcher.dropped(), 3);
    }

    #[test]
    fn a_decoder_error_waits_for_the_next_keyframe() {
        let watcher = Watcher::default();
        let now = Instant::now();
        watcher.push(unit(true, 1), now);
        assert_eq!(drain(&watcher), [(1, false)]);
        watcher.wait_for_keyframe();
        watcher.push(unit(false, 2), now);
        watcher.push(unit(true, 3), now);
        assert_eq!(drain(&watcher), [(3, true)]);
    }

    #[test]
    fn a_token_opens_once_and_revoking_closes_the_socket() {
        let hub = Hub::default();
        hub.allow("t", "s");
        let (source, watcher) = hub.open("t").unwrap();
        assert_eq!(source, "s");
        assert!(hub.open("t").is_none(), "once");
        assert!(hub.watched("s"));
        hub.revoke("t");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        assert!(runtime.block_on(watcher.next()).is_none());
        assert!(!hub.watched("s"));
        assert!(hub.open("never").is_none());
    }
}

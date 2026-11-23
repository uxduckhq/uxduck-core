//! Who watches which source, and each watcher's queue (ADR-0017 V6, V7).
//! A watcher starts at a keyframe; when its oldest queued picture is more
//! than 200 ms old the queue is dropped and it waits for the next keyframe,
//! marked as a discontinuity, so a slow client freezes briefly and catches
//! up rather than falling behind.
//!
//! Sound (ADR-0067 M8, M11) goes only to the sockets that said `sound on`,
//! after their first picture, in the same queue as the pictures, so it
//! waits behind them as the app plays it. A drop at 200 ms drops the
//! queued sound with the pictures, since it is as late as they are, but
//! sound keeps coming while the pictures wait for their next keyframe:
//! it needs none. The first sound after a drop is marked as one. A
//! source's sound is wanted while one of its sockets listens or a
//! recording takes it, and its worker hears that from `sound_wanted`.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use uxduck_media::aac::AudioUnit;
use uxduck_media::au::AccessUnit;
use uxduck_media::latency::Window;

/// How much a watcher may fall behind (V7).
pub const MAX_QUEUED: Duration = Duration::from_millis(200);
/// A token not used within this long is forgotten.
const TOKEN_LIFETIME: Duration = Duration::from_secs(30);
/// Recovery points a browser waits through, with no IDR, before the camera
/// is re-encoded for it: a camera with periodic intra refresh sends no IDR
/// after it starts, and `WebCodecs` starts only at one.
pub const RECOVERIES_BEFORE_REENCODE: u32 = 2;
/// After a failed re-encode, how long before trying again.
const REENCODE_RETRY: Duration = Duration::from_secs(60);

/// One thing to send.
#[derive(Clone, Debug)]
pub enum Item {
    Unit(Arc<AccessUnit>),
    Klv {
        timestamp_us: u64,
        data: Arc<[u8]>,
    },
    /// A frame of the camera's sound: for recordings (S147), and for the
    /// sockets that listen (ADR-0067 M11).
    Audio(Arc<AudioUnit>),
    /// A camera's MPEG-TS bytes as they arrived, ahead of the pictures
    /// they bring: for recordings only, which keep them (S151).
    Ts(Arc<[u8]>),
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
    /// Starts at a recovery point too: `FFmpeg` can, `WebCodecs` cannot.
    from_recovery: bool,
    /// Recovery points passed by while waiting for an IDR.
    recoveries_waited: u32,
    closed: bool,
    dropped: u64,
    sent: u64,
    /// The socket said `sound on` and not `sound off` since.
    sound: bool,
    /// Each `sound on` counts one, so the socket says the sound's codec
    /// again with the first sound after it.
    sound_epoch: u64,
    /// The next sound follows dropped sound.
    sound_gap: bool,
}

/// One `/video` socket's queue.
#[derive(Debug)]
pub struct Watcher {
    queue: Mutex<Queue>,
    ready: tokio::sync::Notify,
    /// How long each picture sent lately was held, from its first
    /// datagram's arrival to its send (S262).
    held: Mutex<Window>,
}

impl Default for Watcher {
    fn default() -> Self {
        Self {
            queue: Mutex::new(Queue {
                skipping: true,
                ..Queue::default()
            }),
            ready: tokio::sync::Notify::new(),
            held: Mutex::default(),
        }
    }
}

impl Watcher {
    /// A watcher that may also start at a recovery point (intra refresh).
    fn from_recovery() -> Self {
        let watcher = Self::default();
        watcher.lock().from_recovery = true;
        watcher
    }

    /// A browser's socket, not one of this daemon's own `FFmpeg` taps.
    fn browser(&self) -> bool {
        !self.lock().from_recovery
    }

    /// A browser still waiting for an IDR after recovery points went by.
    fn waits_on_refresh(&self) -> bool {
        let q = self.lock();
        !q.from_recovery && q.skipping && q.recoveries_waited >= RECOVERIES_BEFORE_REENCODE
    }

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
            q.recoveries_waited = 0;
            q.sound_gap = true;
        }
        if matches!(item, Item::Audio(_)) {
            // Sound waits for the first picture, whose clock it plays on.
            if !(q.sound && q.started) {
                return;
            }
        } else if let Item::Unit(unit) = &item {
            if q.skipping {
                if !(unit.key || (q.from_recovery && unit.recovery)) {
                    q.recoveries_waited += u32::from(unit.recovery);
                    q.dropped += 1;
                    return;
                }
                q.skipping = false;
                q.recoveries_waited = 0;
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
        q.recoveries_waited = 0;
    }

    pub fn close(&self) {
        self.lock().closed = true;
        self.ready.notify_one();
    }

    /// The socket said `sound on` or `sound off` (ADR-0067 M11). Sound
    /// still queued when it stops is not sent.
    fn listen(&self, on: bool) {
        let mut q = self.lock();
        if q.sound == on {
            return;
        }
        q.sound = on;
        q.sound_gap = false;
        if on {
            q.sound_epoch += 1;
        } else {
            q.items.retain(|(_, item)| !matches!(item, Item::Audio(_)));
        }
    }

    fn listening(&self) -> bool {
        self.lock().sound
    }

    /// Which `sound on` the socket is in: its codec is said again with the
    /// first sound after each.
    pub fn sound_epoch(&self) -> u64 {
        self.lock().sound_epoch
    }

    fn held(&self) -> std::sync::MutexGuard<'_, Window> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A picture went out on the socket `now`: how long it was held.
    pub fn sent(&self, unit: &AccessUnit, now: Instant) {
        if let Some(arrived) = unit.arrived {
            let held = now.saturating_duration_since(arrived).as_micros();
            self.held()
                .add(now, i64::try_from(held).unwrap_or(i64::MAX));
        }
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
                    let gap = match item {
                        Item::Unit(_) => std::mem::take(&mut q.discontinuity),
                        Item::Audio(_) => std::mem::take(&mut q.sound_gap),
                        _ => false,
                    };
                    return Some((item, gap));
                }
            }
            self.ready.notified().await;
        }
    }
}

#[derive(Debug, Default)]
struct Inner {
    /// Sources someone needs, and since when.
    needed: HashMap<String, Instant>,
    /// Tokens the engine handed out, not yet opened.
    allowed: HashMap<String, (String, Instant)>,
    /// Open sockets by token: their source and queue.
    open: HashMap<String, (String, Arc<Watcher>)>,
    /// Recordings by id: their source, and their writer's end (S40).
    recorders: HashMap<String, (String, std::sync::mpsc::Sender<Item>)>,
    /// Playback tokens by token.
    playback: HashMap<String, Playback>,
    /// Photo tokens (S144): the photo's id and file, and when given.
    photos: HashMap<String, (String, String, Instant)>,
    /// Thumbnail tokens: each photo's file by its id, and when given.
    thumbnails: HashMap<String, (HashMap<String, String>, Instant)>,
    /// Sources re-encoded for browsers now, and those whose re-encode
    /// failed, until when not to try again.
    reencoded: std::collections::HashSet<String>,
    reencode_failed: HashMap<String, Instant>,
    /// Whether each source's sound is wanted, for its worker (ADR-0067 M8).
    sound_wanted: HashMap<String, tokio::sync::watch::Sender<bool>>,
    /// Sources the engine records or is about to: their sound is wanted
    /// before the recording takes its first picture, so a camera is not
    /// opened again for it once the pictures come (ADR-0067 M8, M10).
    recording_asked: std::collections::HashSet<String>,
}

impl Inner {
    /// Whether a source's sound is wanted now: one of its sockets listens,
    /// or a recording takes it or is asked of it (ADR-0067 M8).
    fn wants_sound(&self, source_id: &str) -> bool {
        self.open
            .values()
            .any(|(s, w)| s == source_id && w.listening())
            || self.recorders.values().any(|(s, _)| s == source_id)
            || self.recording_asked.contains(source_id)
    }

    /// Tells a source's worker when whether its sound is wanted changed.
    fn rethink_sound(&self, source_id: &str) {
        if let Some(wanted) = self.sound_wanted.get(source_id) {
            let now = self.wants_sound(source_id);
            wanted.send_if_modified(|was| std::mem::replace(was, now) != now);
        }
    }
}

/// How long a playback token lasts.
const PLAYBACK_LIFETIME: Duration = Duration::from_secs(3600);

/// Why a thumbnail token gives no photo.
#[derive(Debug, PartialEq, Eq)]
pub enum ThumbnailRefused {
    /// Unknown, or older than a photo's token lasts.
    Lapsed,
    /// It names other photos.
    NotNamed,
}

/// A playback token: the recording, where its bytes are, whether it was
/// still being written when the token was given and whether the token has
/// had its own copy of it (FB-243), when it was given, when a response
/// with its bytes last finished, and how many are being sent now.
#[derive(Debug)]
struct Playback {
    recording_id: String,
    at: uxduck_engine::runtime::VideoAt,
    still_written: bool,
    had_copy: bool,
    given: Instant,
    asked: Instant,
    sending: usize,
}

/// What a playback token is for, while it lasts (S40, FB-243).
#[derive(Clone, Debug)]
pub struct Playing {
    pub recording_id: String,
    /// Where its bytes are: in the media folder, where it was added from,
    /// or in a `.uxflight` (ADR-0062).
    pub at: uxduck_engine::runtime::VideoAt,
    /// Still being written when the token was given.
    pub still_written: bool,
    /// The token has had its own copy of it.
    pub had_copy: bool,
}

/// A response with a playback token's bytes, while it is being sent: the
/// token's copy is in use until it is dropped (FB-243).
pub struct Sending {
    hub: Arc<Hub>,
    token: String,
}

impl Drop for Sending {
    fn drop(&mut self) {
        if let Some(p) = self.hub.lock().playback.get_mut(&self.token) {
            p.sending = p.sending.saturating_sub(1);
            p.asked = Instant::now();
        }
    }
}

/// Shared by the listener, the `/video` route and the engine's port.
#[derive(Debug, Default)]
pub struct Hub {
    inner: Mutex<Inner>,
    /// Where to ask for a source to be re-encoded (`reencode::manage`).
    reencoder: Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>,
    /// Where photos and recordings are kept, together (S150): the Video
    /// storage category.
    dir: std::path::PathBuf,
}

impl Hub {
    pub fn demand(&self, source_id: &str, needed: bool) {
        let mut inner = self.lock();
        if needed {
            inner
                .needed
                .entry(source_id.to_owned())
                .or_insert_with(Instant::now);
        } else {
            inner.needed.remove(source_id);
        }
    }

    pub fn needed(&self, source_id: &str) -> bool {
        self.lock().needed.contains_key(source_id)
    }

    /// Since when a source has been needed, while it is.
    pub fn needed_since(&self, source_id: &str) -> Option<Instant> {
        self.lock().needed.get(source_id).copied()
    }

    #[must_use]
    pub fn new(dir: std::path::PathBuf) -> Self {
        Self {
            inner: Mutex::default(),
            reencoder: Mutex::default(),
            dir,
        }
    }

    #[must_use]
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    pub fn allow_photo(&self, token: &str, photo_id: &str, file_name: &str) {
        let mut inner = self.lock();
        let now = Instant::now();
        inner
            .photos
            .retain(|_, (_, _, at)| now.duration_since(*at) < PLAYBACK_LIFETIME);
        inner.photos.insert(
            token.to_owned(),
            (photo_id.to_owned(), file_name.to_owned(), now),
        );
    }

    /// `/thumbnails/<photo id>.jpg?token=` for each photo named, by its id
    /// and file, for as long as a photo's token lasts.
    pub fn allow_thumbnails(&self, token: &str, photos: &[(String, String)]) {
        let mut inner = self.lock();
        let now = Instant::now();
        inner
            .thumbnails
            .retain(|_, (_, at)| now.duration_since(*at) < PLAYBACK_LIFETIME);
        inner
            .thumbnails
            .insert(token.to_owned(), (photos.iter().cloned().collect(), now));
    }

    /// The file of a photo a thumbnail token names, while it lasts.
    ///
    /// # Errors
    /// The token is unknown or lapsed, or it does not name the photo.
    pub fn thumbnail(&self, token: &str, photo_id: &str) -> Result<String, ThumbnailRefused> {
        let inner = self.lock();
        let (photos, _) = inner
            .thumbnails
            .get(token)
            .filter(|(_, at)| at.elapsed() < PLAYBACK_LIFETIME)
            .ok_or(ThumbnailRefused::Lapsed)?;
        photos
            .get(photo_id)
            .cloned()
            .ok_or(ThumbnailRefused::NotNamed)
    }

    /// The photo a token is for, while it lasts: its id and file.
    pub fn photo(&self, token: &str) -> Option<(String, String)> {
        let inner = self.lock();
        let (id, file, at) = inner.photos.get(token)?;
        (at.elapsed() < PLAYBACK_LIFETIME).then(|| (id.clone(), file.clone()))
    }

    /// Sends a source's pictures to a recording's writer too. A recording
    /// takes the camera's sound, so it wants it (ADR-0067 M8).
    pub fn record(&self, recording_id: &str, source_id: &str, to: std::sync::mpsc::Sender<Item>) {
        let mut inner = self.lock();
        let old = inner
            .recorders
            .insert(recording_id.to_owned(), (source_id.to_owned(), to));
        inner.rethink_sound(source_id);
        if let Some((old, _)) = old.filter(|(old, _)| old != source_id) {
            inner.rethink_sound(&old);
        }
    }

    /// Whether a source's sound is wanted, now and as it changes: while one
    /// of its sockets listens or a recording takes it (ADR-0067 M8).
    pub fn sound_wanted(&self, source_id: &str) -> tokio::sync::watch::Receiver<bool> {
        let mut inner = self.lock();
        let now = inner.wants_sound(source_id);
        inner
            .sound_wanted
            .entry(source_id.to_owned())
            .or_insert_with(|| tokio::sync::watch::Sender::new(now))
            .subscribe()
    }

    /// Whether the engine records a source or is about to, which wants its
    /// sound from before the recording's first picture (ADR-0067 M8).
    pub fn record_sound(&self, source_id: &str, asked: bool) {
        let mut inner = self.lock();
        let changed = if asked {
            inner.recording_asked.insert(source_id.to_owned())
        } else {
            inner.recording_asked.remove(source_id)
        };
        if changed {
            inner.rethink_sound(source_id);
        }
    }

    /// A socket said `sound on` or `sound off` (ADR-0067 M11).
    pub fn listen(&self, token: &str, on: bool) {
        let inner = self.lock();
        let Some((source_id, watcher)) = inner.open.get(token) else {
            return;
        };
        watcher.listen(on);
        inner.rethink_sound(source_id);
    }

    /// Whether a recording takes this source's items now.
    pub fn recorded(&self, source_id: &str) -> bool {
        self.lock().recorders.values().any(|(s, _)| s == source_id)
    }

    /// A camera's MPEG-TS bytes for its recordings, when it has any and
    /// the bytes are MPEG-TS (raw or inside RTP): before the pictures they
    /// bring are published (S151).
    pub fn publish_ts(&self, source_id: &str, datagram: &[u8]) {
        if !self.recorded(source_id) {
            return;
        }
        if let Ok(uxduck_media::udp::Packet::Ts { ts, .. }) = uxduck_media::udp::classify(datagram)
        {
            self.publish(source_id, &Item::Ts(ts.into()));
        }
    }

    /// Ends a recording: its writer finishes the file.
    pub fn stop_recording(&self, recording_id: &str) {
        let mut inner = self.lock();
        if let Some((source_id, _)) = inner.recorders.remove(recording_id) {
            inner.rethink_sound(&source_id);
        }
    }

    pub fn allow_playback(
        &self,
        token: &str,
        recording_id: &str,
        bytes: uxduck_engine::runtime::VideoAt,
        still_written: bool,
    ) {
        let mut inner = self.lock();
        let now = Instant::now();
        inner
            .playback
            .retain(|_, p| now.duration_since(p.given) < PLAYBACK_LIFETIME);
        inner.playback.insert(
            token.to_owned(),
            Playback {
                recording_id: recording_id.to_owned(),
                at: bytes,
                still_written,
                had_copy: false,
                given: now,
                asked: now,
                sending: 0,
            },
        );
    }

    /// The recording a playback token is for, while it lasts.
    pub fn playback(&self, token: &str) -> Option<Playing> {
        let inner = self.lock();
        let p = inner.playback.get(token)?;
        (p.given.elapsed() < PLAYBACK_LIFETIME).then(|| Playing {
            recording_id: p.recording_id.clone(),
            at: p.at.clone(),
            still_written: p.still_written,
            had_copy: p.had_copy,
        })
    }

    /// A response with a token's bytes begins: its copy is in use until
    /// the returned guard is dropped, when the response has been sent or
    /// the player went away.
    pub fn sending(self: &Arc<Self>, token: &str) -> Sending {
        if let Some(p) = self.lock().playback.get_mut(token) {
            p.sending += 1;
        }
        Sending {
            hub: self.clone(),
            token: token.to_owned(),
        }
    }

    /// The token has its own copy now (FB-243).
    pub fn had_copy(&self, token: &str) {
        if let Some(p) = self.lock().playback.get_mut(token) {
            p.had_copy = true;
        }
    }

    /// When a playback token's copy was last in use, while the token
    /// lasts: now while a response is being sent, else when the last one
    /// finished. Idle time counts from there, as a `<video>` can play over
    /// one long response.
    pub fn playback_asked(&self, token: &str) -> Option<Instant> {
        let inner = self.lock();
        let p = inner.playback.get(token)?;
        if p.given.elapsed() >= PLAYBACK_LIFETIME {
            return None;
        }
        Some(if p.sending > 0 {
            Instant::now()
        } else {
            p.asked
        })
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
        if let Some((source_id, watcher)) = inner.open.remove(token) {
            watcher.close();
            inner.rethink_sound(&source_id);
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
        // FFmpeg decodes it, so it can start at a recovery point.
        let watcher = Arc::new(Watcher::from_recovery());
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
        let mut inner = self.lock();
        if let Some((source_id, watcher)) = inner.open.remove(token) {
            watcher.close();
            inner.rethink_sound(&source_id);
        }
    }

    /// Re-encode requests go here from now on.
    pub fn reencode_with(&self, to: tokio::sync::mpsc::UnboundedSender<String>) {
        *self
            .reencoder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(to);
    }

    /// How long the daemon holds this source's pictures, in microseconds:
    /// the median over the last second, the longest of its browsers'
    /// (S262). None while no browser was sent a picture in that second.
    pub fn hold_us(&self, source_id: &str) -> Option<u64> {
        let now = Instant::now();
        self.watchers_of(source_id)
            .iter()
            .filter(|w| w.browser())
            .filter_map(|w| w.held().median(now))
            .max()
            .and_then(|us| u64::try_from(us).ok())
    }

    /// Browsers watching a source now.
    pub fn browsers(&self, source_id: &str) -> usize {
        self.lock()
            .open
            .values()
            .filter(|(s, w)| s == source_id && w.browser())
            .count()
    }

    /// Whether a source is re-encoded for browsers now.
    pub fn reencoded(&self, source_id: &str) -> bool {
        self.lock().reencoded.contains(source_id)
    }

    /// A re-encode ended: the camera's own pictures go to browsers again.
    /// After a failure the source is not re-encoded again for a minute.
    /// Browsers still watching wait for the camera's own IDR: its pictures
    /// do not follow on from the re-encoded ones.
    pub fn reencode_ended(&self, source_id: &str, failed: bool) {
        {
            let mut inner = self.lock();
            inner.reencoded.remove(source_id);
            if failed {
                inner
                    .reencode_failed
                    .insert(source_id.to_owned(), Instant::now() + REENCODE_RETRY);
            }
        }
        for watcher in self.watchers_of(source_id) {
            if watcher.browser() {
                watcher.wait_for_keyframe();
            }
        }
    }

    /// A re-encoded picture: for the source's browsers only.
    pub fn publish_reencoded(&self, source_id: &str, unit: Arc<AccessUnit>) {
        let now = Instant::now();
        let item = Item::Unit(unit);
        for watcher in self.watchers_of(source_id) {
            if watcher.browser() {
                watcher.push(item.clone(), now);
            }
        }
    }

    fn watchers_of(&self, source_id: &str) -> Vec<Arc<Watcher>> {
        self.lock()
            .open
            .values()
            .filter(|(s, _)| s == source_id)
            .map(|(_, w)| w.clone())
            .collect()
    }

    /// A new RTSP session has its own clock and decoder references. Drop
    /// queued pictures/metadata and resume at a fresh keyframe, even when
    /// the next address has exactly the same codec and dimensions.
    pub fn restart_source(&self, source_id: &str) {
        for watcher in self.watchers_of(source_id) {
            let mut q = watcher.lock();
            q.dropped += q.items.len() as u64;
            q.items.clear();
            q.skipping = true;
            q.recoveries_waited = 0;
            q.sound_gap = true;
        }
    }

    /// Asks once for a source to be re-encoded, unless it is already or
    /// failed a moment ago.
    fn want_reencode(&self, source_id: &str, now: Instant) {
        {
            let mut inner = self.lock();
            if inner.reencoded.contains(source_id)
                || inner
                    .reencode_failed
                    .get(source_id)
                    .is_some_and(|until| now < *until)
            {
                return;
            }
            inner.reencoded.insert(source_id.to_owned());
        }
        let sent = self
            .reencoder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|to| to.send(source_id.to_owned()).is_ok());
        if !sent {
            self.lock().reencoded.remove(source_id);
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
        let watchers = self.watchers_of(source_id);
        // Sockets and the live publisher take pictures and KLV only.
        if matches!(item, Item::Unit(_) | Item::Klv { .. }) {
            // A re-encoded camera's browsers take the re-encoder's pictures.
            let reencoded = matches!(item, Item::Unit(_)) && self.reencoded(source_id);
            for watcher in &watchers {
                if !(reencoded && watcher.browser()) {
                    watcher.push(item.clone(), now);
                }
            }
            if let Item::Unit(unit) = item
                && unit.recovery
                && !reencoded
                && watchers.iter().any(|w| w.waits_on_refresh())
            {
                self.want_reencode(source_id, now);
            }
        }
        // Sound goes to the sockets that listen, which their queues decide.
        if matches!(item, Item::Audio(_)) {
            for watcher in &watchers {
                watcher.push(item.clone(), now);
            }
        }
        // Recordings get every picture: no dropping to catch up.
        let mut inner = self.lock();
        let before = inner.recorders.len();
        inner
            .recorders
            .retain(|_, (source, to)| source != source_id || to.send(item.clone()).is_ok());
        if inner.recorders.len() != before {
            inner.rethink_sound(source_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uxduck_media::nal::SpsInfo;

    fn unit(key: bool, timestamp_us: u64) -> Item {
        Item::Unit(Arc::new(AccessUnit {
            arrived: None,
            data: vec![0, 0, 0, 1],
            key,
            recovery: false,
            timestamp_us,
            composition_us: 0,
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
    fn switching_addresses_resets_only_that_cameras_watchers() {
        let hub = Hub::default();
        hub.allow("front", "camera");
        let (_, browser) = hub.open("front").unwrap();
        let publisher = hub.tap("publisher", "camera");
        let other = hub.tap("rear", "other-camera");
        hub.publish("camera", &unit(true, 100));
        assert_eq!(drain(&browser), [(100, false)]);
        assert_eq!(drain(&publisher), [(100, false)]);
        hub.publish("camera", &unit(false, 101));
        hub.publish(
            "camera",
            &Item::Klv {
                timestamp_us: 101,
                data: vec![1].into(),
            },
        );
        hub.publish("other-camera", &unit(true, 200));
        hub.restart_source("camera");
        assert!(browser.lock().items.is_empty());
        // The fallback clock starts over and may begin between keyframes.
        hub.publish("camera", &unit(false, 0));
        hub.publish("camera", &unit(true, 1));
        assert_eq!(drain(&browser), [(1, true)]);
        assert_eq!(drain(&publisher), [(1, true)]);
        assert_eq!(drain(&other), [(200, false)]);
    }

    fn recovery(timestamp_us: u64) -> Item {
        let Item::Unit(unit) = unit(false, timestamp_us) else {
            unreachable!()
        };
        Item::Unit(Arc::new(AccessUnit {
            recovery: true,
            ..(*unit).clone()
        }))
    }

    #[test]
    fn a_browser_waits_for_an_idr_but_ffmpeg_starts_at_a_recovery_point() {
        let hub = Hub::default();
        hub.allow("socket", "camera");
        let (_, socket) = hub.open("socket").unwrap();
        let live = hub.tap("viewer", "camera");
        for item in [unit(false, 1), recovery(2), unit(false, 3), unit(true, 4)] {
            hub.publish("camera", &item);
        }
        // WebCodecs refuses anything but an IDR to start from.
        assert_eq!(drain(&socket), [(4, false)]);
        assert_eq!(drain(&live), [(2, false), (3, false), (4, false)]);
    }

    #[test]
    fn a_browser_waiting_through_refreshes_has_the_camera_re_encoded_once() {
        let hub = Hub::default();
        let (to, mut asked) = tokio::sync::mpsc::unbounded_channel();
        hub.reencode_with(to);
        hub.allow("socket", "camera");
        let (_, browser) = hub.open("socket").unwrap();
        let live = hub.tap("viewer", "camera");
        hub.publish("camera", &recovery(1));
        assert!(asked.try_recv().is_err(), "one refresh is not enough");
        hub.publish("camera", &unit(false, 2));
        hub.publish("camera", &recovery(3));
        assert_eq!(asked.try_recv().unwrap(), "camera");
        assert!(hub.reencoded("camera"));
        // The camera's own pictures go on to FFmpeg, not to the browser,
        // which takes the re-encoder's from its IDR.
        hub.publish("camera", &recovery(4));
        let Item::Unit(idr) = unit(true, 5) else {
            unreachable!()
        };
        hub.publish_reencoded("camera", idr);
        assert_eq!(drain(&browser), [(5, false)]);
        assert_eq!(
            drain(&live),
            [(1, false), (2, false), (3, false), (4, false)]
        );
        // Asked once while it lasts.
        hub.publish("camera", &recovery(6));
        assert!(asked.try_recv().is_err());
        assert_eq!(hub.browsers("camera"), 1);
    }

    #[test]
    fn a_failed_re_encode_waits_before_asking_again() {
        let hub = Hub::default();
        let (to, mut asked) = tokio::sync::mpsc::unbounded_channel();
        hub.reencode_with(to);
        hub.allow("socket", "camera");
        let _browser = hub.open("socket").unwrap();
        for t in 1..=2 {
            hub.publish("camera", &recovery(t));
        }
        assert!(asked.try_recv().is_ok());
        hub.reencode_ended("camera", true);
        assert!(!hub.reencoded("camera"));
        hub.publish("camera", &recovery(3));
        assert!(asked.try_recv().is_err());
    }

    #[test]
    fn separate_waits_do_not_add_up_to_a_re_encode() {
        let hub = Hub::default();
        let (to, mut asked) = tokio::sync::mpsc::unbounded_channel();
        hub.reencode_with(to);
        hub.allow("socket", "camera");
        let (_, browser) = hub.open("socket").unwrap();
        // Each wait passes one recovery point before the camera's IDR.
        for round in 0..3 {
            let t = round * 10;
            hub.publish("camera", &recovery(t + 1));
            hub.publish("camera", &unit(true, t + 2));
            browser.wait_for_keyframe();
        }
        assert!(asked.try_recv().is_err());
    }

    #[test]
    fn browsers_wait_for_the_cameras_own_idr_when_a_re_encode_ends() {
        let hub = Hub::default();
        let (to, _asked) = tokio::sync::mpsc::unbounded_channel();
        hub.reencode_with(to);
        hub.allow("socket", "camera");
        let (_, browser) = hub.open("socket").unwrap();
        for t in 1..=2 {
            hub.publish("camera", &recovery(t));
        }
        let Item::Unit(idr) = unit(true, 3) else {
            unreachable!()
        };
        hub.publish_reencoded("camera", idr);
        assert_eq!(drain(&browser), [(3, false)]);
        hub.reencode_ended("camera", true);
        // The camera's pictures do not follow on from the re-encoded ones.
        hub.publish("camera", &unit(false, 4));
        hub.publish("camera", &recovery(5));
        hub.publish("camera", &unit(true, 6));
        assert_eq!(drain(&browser), [(6, true)]);
    }

    #[test]
    fn a_camera_with_idrs_is_never_re_encoded() {
        let hub = Hub::default();
        let (to, mut asked) = tokio::sync::mpsc::unbounded_channel();
        hub.reencode_with(to);
        hub.allow("socket", "camera");
        let (_, browser) = hub.open("socket").unwrap();
        hub.publish("camera", &recovery(1));
        hub.publish("camera", &unit(true, 2));
        for t in 3..=6 {
            hub.publish("camera", &recovery(t));
        }
        assert!(asked.try_recv().is_err());
        assert_eq!(drain(&browser).len(), 5);
    }

    #[test]
    fn sound_is_recorded_but_never_queued_for_live_viewers() {
        let hub = Hub::default();
        let watcher = hub.tap("viewer", "camera");
        let (to, from) = std::sync::mpsc::channel();
        hub.record("recording", "camera", to);
        hub.publish("camera", &unit(true, 0));
        let _ = from.try_recv().unwrap();
        let _ = drain(&watcher);
        hub.publish(
            "camera",
            &Item::Audio(Arc::new(AudioUnit::aac(
                vec![1, 2, 3],
                0,
                uxduck_media::aac::AacConfig {
                    object_type: 2,
                    frequency_index: 3,
                    sample_rate: 48_000,
                    channels: 2,
                },
            ))),
        );
        assert!(matches!(from.try_recv().unwrap(), Item::Audio(_)));
        assert!(watcher.lock().items.is_empty());
    }

    fn sound(timestamp_us: u64) -> Item {
        Item::Audio(Arc::new(AudioUnit {
            data: vec![0xFC],
            timestamp_us,
            config: uxduck_media::audio::AudioConfig::Opus(uxduck_media::opus::OpusConfig {
                channels: 1,
            }),
            samples: 960,
        }))
    }

    /// Everything queued: pictures as `p`, sound as `s`, KLV as `k`, with
    /// its time and whether it follows a gap.
    fn drain_all(watcher: &Watcher) -> Vec<(char, u64, bool)> {
        let mut out = Vec::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        while !watcher.lock().items.is_empty() {
            match runtime.block_on(watcher.next()) {
                Some((Item::Unit(u), gap)) => out.push(('p', u.timestamp_us, gap)),
                Some((Item::Audio(a), gap)) => out.push(('s', a.timestamp_us, gap)),
                Some((Item::Klv { timestamp_us, .. }, gap)) => out.push(('k', timestamp_us, gap)),
                _ => {}
            }
        }
        out
    }

    /// ADR-0067 M8: a source's sound is wanted while one of its sockets
    /// listens or a recording takes it, and its worker hears each change.
    #[test]
    fn sound_is_wanted_while_a_socket_listens_or_a_recording_takes_it_or_is_asked() {
        let hub = Hub::default();
        let mut wanted = hub.sound_wanted("camera");
        assert!(!*wanted.borrow_and_update());
        for (token, source) in [("a", "camera"), ("b", "camera"), ("c", "other")] {
            hub.allow(token, source);
            let _ = hub.open(token).unwrap();
        }
        hub.listen("c", true);
        assert!(!wanted.has_changed().unwrap(), "another camera's socket");
        hub.listen("a", true);
        assert!(wanted.has_changed().unwrap());
        assert!(*wanted.borrow_and_update());
        hub.listen("b", true);
        hub.listen("a", false);
        assert!(!wanted.has_changed().unwrap(), "b still listens");
        hub.close("b");
        assert!(!*wanted.borrow_and_update(), "the last listener closed");
        let (to, _from) = std::sync::mpsc::channel();
        hub.record("r", "camera", to);
        assert!(*wanted.borrow_and_update(), "recordings keep the sound");
        hub.stop_recording("r");
        assert!(!*wanted.borrow_and_update());
        // A recording whose writer ended stops wanting it at the next item.
        let (to, from) = std::sync::mpsc::channel();
        hub.record("r2", "camera", to);
        assert!(*wanted.borrow_and_update());
        drop(from);
        hub.publish("camera", &unit(true, 0));
        assert!(!*wanted.borrow_and_update());
        // A socket revoked with its watch.
        hub.listen("a", true);
        assert!(*wanted.borrow_and_update());
        hub.revoke("a");
        assert!(!*wanted.borrow_and_update());
        assert!(*hub.sound_wanted("other").borrow(), "c listens to its own");
        // A recording the engine asks for wants it before its first picture.
        hub.record_sound("camera", true);
        assert!(*wanted.borrow_and_update(), "a recording on its way");
        hub.record_sound("camera", true);
        assert!(!wanted.has_changed().unwrap());
        hub.record_sound("camera", false);
        assert!(!*wanted.borrow_and_update());
    }

    /// ADR-0067 M11: sound goes only to the sockets that said `sound on`,
    /// once they had their first picture, and none is sent after `sound off`.
    #[test]
    fn sound_goes_only_to_sockets_that_listen_after_their_first_picture() {
        let hub = Hub::default();
        hub.allow("listens", "camera");
        hub.allow("quiet", "camera");
        let (_, listens) = hub.open("listens").unwrap();
        let (_, quiet) = hub.open("quiet").unwrap();
        let tap = hub.tap("publisher", "camera");
        hub.listen("listens", true);
        hub.publish("camera", &sound(0));
        hub.publish("camera", &unit(true, 10));
        hub.publish("camera", &sound(20));
        assert_eq!(drain_all(&listens), [('p', 10, false), ('s', 20, false)]);
        assert_eq!(drain_all(&quiet), [('p', 10, false)]);
        assert_eq!(drain_all(&tap), [('p', 10, false)]);
        let epoch = listens.sound_epoch();
        hub.publish("camera", &sound(40));
        hub.listen("listens", false);
        hub.publish("camera", &sound(60));
        assert!(drain_all(&listens).is_empty(), "queued sound is dropped");
        hub.listen("listens", true);
        assert_eq!(listens.sound_epoch(), epoch + 1, "the codec is said again");
        hub.publish("camera", &sound(80));
        assert_eq!(drain_all(&listens), [('s', 80, false)]);
    }

    /// A socket 200 ms behind drops its queued sound with the pictures, and
    /// the sound after it is marked; sound keeps coming while the pictures
    /// wait for their keyframe.
    #[test]
    fn sound_after_a_drop_is_marked_and_comes_while_pictures_wait() {
        let watcher = Watcher::default();
        watcher.listen(true);
        let start = Instant::now();
        watcher.push(unit(true, 1), start);
        watcher.push(sound(2), start);
        let late = start + Duration::from_millis(250);
        watcher.push(sound(3), late);
        watcher.push(unit(false, 4), late);
        watcher.push(sound(5), late);
        watcher.push(unit(true, 6), late);
        assert_eq!(
            drain_all(&watcher),
            [('s', 3, true), ('s', 5, false), ('p', 6, true)]
        );
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

    /// What the daemon holds a picture for (S262): per browser, the median
    /// of the last second's; the longest browser's for the source. The
    /// daemon's own taps and pictures with no arrival are not counted.
    #[test]
    fn the_hold_is_the_slowest_browsers_median_over_the_last_second() {
        let hub = Hub::default();
        assert_eq!(hub.hold_us("camera"), None);
        hub.allow("fast", "camera");
        hub.allow("slow", "camera");
        let (_, fast) = hub.open("fast").unwrap();
        let (_, slow) = hub.open("slow").unwrap();
        let tap = hub.tap("publisher", "camera");
        let now = Instant::now();
        let picture = |held_ms: u64| {
            let Item::Unit(unit) = unit(true, 0) else {
                unreachable!()
            };
            AccessUnit {
                arrived: now.checked_sub(Duration::from_millis(held_ms)),
                ..(*unit).clone()
            }
        };
        assert_eq!(hub.hold_us("camera"), None, "nothing sent yet");
        for held in [4, 5, 6] {
            fast.sent(&picture(held), now);
        }
        for held in [30, 90, 40] {
            slow.sent(&picture(held), now);
        }
        tap.sent(&picture(900), now);
        let Item::Unit(made) = unit(true, 0) else {
            unreachable!()
        };
        slow.sent(&made, now);
        assert_eq!(hub.hold_us("camera"), Some(40_000));
        assert_eq!(hub.hold_us("other"), None);
        hub.close("slow");
        assert_eq!(hub.hold_us("camera"), Some(5_000));
    }

    /// FB-243: a token's copy is in use while a response is still being
    /// sent, so a `<video>` playing over one long request never looks idle.
    #[test]
    fn a_tokens_copy_is_in_use_until_its_response_is_sent() {
        let hub = Arc::new(Hub::new(std::path::PathBuf::from("/media")));
        let at = uxduck_engine::runtime::VideoAt::Media {
            file: "r.ts".into(),
        };
        hub.allow_playback("vp1", "vr1", at, true);
        let given = hub.playback_asked("vp1").unwrap();
        let playing = hub.playback("vp1").unwrap();
        assert!(playing.still_written && !playing.had_copy);
        let sending = hub.sending("vp1");
        hub.had_copy("vp1");
        assert!(hub.playback("vp1").unwrap().had_copy);
        std::thread::sleep(Duration::from_millis(20));
        let during = hub.playback_asked("vp1").unwrap();
        assert!(during.duration_since(given) >= Duration::from_millis(20));
        drop(sending);
        let sent = hub.playback_asked("vp1").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        // Idle from when it was sent, not from now.
        assert_eq!(hub.playback_asked("vp1"), Some(sent));
        assert_eq!(hub.playback_asked("vp2"), None);
    }
}

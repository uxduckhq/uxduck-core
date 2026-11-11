//! Who watches which source, and each watcher's queue (ADR-0017 V6, V7).
//! A watcher starts at a keyframe; when its oldest queued picture is more
//! than 200 ms old the queue is dropped and it waits for the next keyframe,
//! marked as a discontinuity, so a slow client freezes briefly and catches
//! up rather than falling behind.

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
    /// A frame of the camera's sound: for recordings only (S147).
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
        }
        if let Item::Unit(unit) = &item {
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
    needed: std::collections::HashSet<String>,
    /// Tokens the engine handed out, not yet opened.
    allowed: HashMap<String, (String, Instant)>,
    /// Open sockets by token: their source and queue.
    open: HashMap<String, (String, Arc<Watcher>)>,
    /// Recordings by id: their source, and their writer's end (S40).
    recorders: HashMap<String, (String, std::sync::mpsc::Sender<Item>)>,
    /// Playback tokens: the recording and its file, and when given.
    playback: HashMap<String, (String, String, Instant)>,
    /// Photo tokens (S144): the photo's id and file, and when given.
    photos: HashMap<String, (String, String, Instant)>,
    /// Sources re-encoded for browsers now, and those whose re-encode
    /// failed, until when not to try again.
    reencoded: std::collections::HashSet<String>,
    reencode_failed: HashMap<String, Instant>,
}

/// How long a playback token lasts.
const PLAYBACK_LIFETIME: Duration = Duration::from_secs(3600);

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
            inner.needed.insert(source_id.to_owned());
        } else {
            inner.needed.remove(source_id);
        }
    }

    pub fn needed(&self, source_id: &str) -> bool {
        self.lock().needed.contains(source_id)
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

    /// The photo a token is for, while it lasts: its id and file.
    pub fn photo(&self, token: &str) -> Option<(String, String)> {
        let inner = self.lock();
        let (id, file, at) = inner.photos.get(token)?;
        (at.elapsed() < PLAYBACK_LIFETIME).then(|| (id.clone(), file.clone()))
    }

    /// Sends a source's pictures to a recording's writer too.
    pub fn record(&self, recording_id: &str, source_id: &str, to: std::sync::mpsc::Sender<Item>) {
        self.lock()
            .recorders
            .insert(recording_id.to_owned(), (source_id.to_owned(), to));
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
        self.lock().recorders.remove(recording_id);
    }

    pub fn allow_playback(&self, token: &str, recording_id: &str, file: &str) {
        let mut inner = self.lock();
        let now = Instant::now();
        inner
            .playback
            .retain(|_, (_, _, at)| now.duration_since(*at) < PLAYBACK_LIFETIME);
        inner.playback.insert(
            token.to_owned(),
            (recording_id.to_owned(), file.to_owned(), now),
        );
    }

    /// The recording a playback token is for, while it lasts: its id and
    /// its file in the media folder.
    pub fn playback(&self, token: &str) -> Option<(String, String)> {
        let inner = self.lock();
        let (id, file, at) = inner.playback.get(token)?;
        (at.elapsed() < PLAYBACK_LIFETIME).then(|| (id.clone(), file.clone()))
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
        if let Some((_, watcher)) = self.lock().open.remove(token) {
            watcher.close();
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
}

//! The MAVLink camera protocol's identification (ADR-0063 Y5, S404), and
//! its video streams (ADR-0017 V3, V5, S39).
//!
//! A camera component that announces itself, and every autopilot, is asked
//! for `CAMERA_INFORMATION`: three times with `MAV_CMD_REQUEST_MESSAGE`, then
//! three times with the command it superseded. A camera component speaks
//! for itself. An autopilot speaks for up to six cameras attached to it,
//! each with a `camera_device_id` from 1 to 6. A camera that has video
//! streams is asked for `VIDEO_STREAM_INFORMATION` the same way, and a camera
//! component is asked for it at its first heartbeat as well, since older
//! cameras name a stream without saying what they are.
//!
//! A camera whose heartbeats stop is kept, marked gone. When they return it
//! is identified again from the beginning.

use std::collections::BTreeMap;

/// `CAMERA_INFORMATION`'s message id.
pub const CAMERA_INFORMATION: u32 = 259;
/// `VIDEO_STREAM_INFORMATION`'s message id.
pub const VIDEO_STREAM_INFORMATION: u32 = 269;
/// `VIDEO_STREAM_STATUS`'s message id.
pub const VIDEO_STREAM_STATUS: u32 = 270;
/// `MAV_CMD_REQUEST_CAMERA_INFORMATION`, which `MAV_CMD_REQUEST_MESSAGE`
/// superseded.
pub const REQUEST_CAMERA_INFORMATION: u16 = 521;
/// `MAV_CMD_REQUEST_VIDEO_STREAM_INFORMATION`, superseded likewise.
pub const REQUEST_VIDEO_STREAM_INFORMATION: u16 = 2504;
/// `CAMERA_CAP_FLAGS_HAS_VIDEO_STREAM`.
pub const HAS_VIDEO_STREAM: u32 = 256;
/// The most cameras an autopilot speaks for: MAVLink numbers them 1 to 6
/// in `camera_device_id`, and 0 is a camera that speaks for itself.
pub const AUTOPILOT_CAMERAS: u8 = 6;
const ATTEMPTS: u8 = 3;
const RETRY_US: u64 = 3_000_000;

/// A camera component or an autopilot by
/// `(link, peer, system id, component id)`.
pub type Key = (String, String, u8, u8);

/// What a camera says of itself (`CAMERA_INFORMATION`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Information {
    pub vendor: String,
    pub model: String,
    /// Dev, patch, minor and major from the top byte down. 0 when unknown.
    pub firmware_version: u32,
    pub focal_length_mm: Option<f32>,
    /// Across and down, in millimetres.
    pub sensor_size_mm: Option<(f32, f32)>,
    /// Across and down, in pixels.
    pub resolution_px: Option<(u16, u16)>,
    pub lens_id: u8,
    /// `CAMERA_CAP_FLAGS`.
    pub flags: u32,
    pub definition_version: u16,
    /// Where its camera definition file is, or empty.
    pub definition_uri: String,
    /// The gimbal it sits on: a gimbal device's component id, 1 to 6 for
    /// one its autopilot speaks for, or 0 for none.
    pub gimbal_device_id: u8,
    /// 1 to 6 for a camera an autopilot speaks for, 0 for a camera
    /// component's own.
    pub camera_device_id: u8,
}

impl Information {
    /// The same camera as `other`, as far as its words go: an autopilot
    /// that passes a camera component's answer on changes none of these.
    fn names(&self, other: &Self) -> bool {
        (!self.vendor.is_empty() || !self.model.is_empty())
            && self.vendor == other.vendor
            && self.model == other.model
            && self.firmware_version == other.firmware_version
    }
}

/// One of a camera's video streams (`VIDEO_STREAM_INFORMATION`), brought
/// up to date by `VIDEO_STREAM_STATUS`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stream {
    pub stream_id: u8,
    /// `VIDEO_STREAM_TYPE`: 0 RTSP, 1 RTP over UDP, 2 MPEG-TS over TCP,
    /// 3 MPEG-TS over UDP.
    pub kind: u8,
    /// `VIDEO_STREAM_STATUS_FLAGS`: 1 running, 2 thermal.
    pub flags: u16,
    pub frame_rate_hz: f32,
    pub width: u16,
    pub height: u16,
    pub bitrate: u32,
    pub rotation: u16,
    pub hfov: u16,
    pub name: String,
    pub uri: String,
    /// `VIDEO_STREAM_ENCODING`: 0 unknown, 1 H.264, 2 H.265.
    pub encoding: u8,
    pub camera_device_id: u8,
}

/// A stream as it is now (`VIDEO_STREAM_STATUS`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StreamStatus {
    pub stream_id: u8,
    pub flags: u16,
    pub frame_rate_hz: f32,
    pub width: u16,
    pub height: u16,
    pub bitrate: u32,
    pub rotation: u16,
    pub hfov: u16,
    pub camera_device_id: u8,
}

/// A camera of a vehicle as it is now known (S404).
#[derive(Clone, Debug, PartialEq)]
pub struct Observation {
    pub vehicle_id: String,
    /// The camera's own component, or the autopilot's that speaks for it.
    pub component_id: u8,
    /// 1 to 6 for a camera an autopilot speaks for.
    pub device_id: Option<u8>,
    /// Whether its heartbeats, or its autopilot's, are heard.
    pub present: bool,
    /// None until it says what it is.
    pub information: Option<Information>,
    pub streams: Vec<Stream>,
}

impl Observation {
    /// Whether this is the better account of a camera heard at two
    /// addresses: the one heard, then the one that said what it is.
    #[must_use]
    pub fn better_than(&self, other: &Self) -> bool {
        (self.present, self.information.is_some()) > (other.present, other.information.is_some())
    }
}

/// What to send a camera component or an autopilot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    /// `MAV_CMD_REQUEST_MESSAGE` for a message id.
    Message(u32),
    /// A command that `MAV_CMD_REQUEST_MESSAGE` superseded.
    Superseded(u16),
}

impl Request {
    /// The superseded command's first parameter: 1 asks a camera for its
    /// information, and 0 asks for every stream.
    #[must_use]
    pub fn first_parameter(self) -> f32 {
        if self == Self::Superseded(REQUEST_CAMERA_INFORMATION) {
            1.0
        } else {
            0.0
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Ask {
    attempts: u8,
    last_us: u64,
    answered: bool,
}

impl Ask {
    /// Counts an ask if one is due and fewer than `limit` were made: which
    /// one it is, from 1.
    fn take(&mut self, now_us: u64, limit: u8) -> Option<u8> {
        let waited = self.attempts == 0 || now_us.saturating_sub(self.last_us) >= RETRY_US;
        if self.answered || self.attempts >= limit || !waited {
            return None;
        }
        self.attempts += 1;
        self.last_us = now_us;
        Some(self.attempts)
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Camera {
    pub(crate) information: Option<Information>,
    streams: BTreeMap<u8, Stream>,
    /// What it says of itself, and the photos it took (S405).
    pub(crate) control: crate::camera_control::Control,
}

impl Camera {
    /// `VIDEO_STREAM_STATUS_FLAGS` of all its streams together.
    pub(crate) fn stream_flags(&self) -> u16 {
        self.streams
            .values()
            .fold(0, |all, stream| all | stream.flags)
    }
}

/// A component that says what cameras there are: a camera, or an autopilot.
#[derive(Clone, Debug, Default)]
pub(crate) struct Source {
    pub(crate) autopilot: bool,
    heartbeat_us: u64,
    pub(crate) gone: bool,
    information: Ask,
    /// Asked for its streams once a camera of its says it has some.
    streams: Option<Ask>,
    /// Asked once for how its streams are, after it names one.
    status_asked: bool,
    /// By device id: 0 a camera component itself, 1 to 6 an autopilot's.
    pub(crate) cameras: BTreeMap<u8, Camera>,
    /// What is read back from it once a camera of its can be worked (S405).
    pub(crate) reads: crate::camera_control::Reads,
    /// What it is asked so that its tracking and its thermal range are
    /// known (ADR-0063 Y5).
    pub(crate) watch: crate::camera_tracking::Watch,
}

impl Source {
    /// Whether its own heartbeat came no longer than `within_us` ago.
    pub(crate) fn heard_within(&self, now_us: u64, within_us: u64) -> bool {
        now_us.saturating_sub(self.heartbeat_us) <= within_us
    }
}

#[derive(Clone, Debug, Default)]
pub struct Cameras {
    /// Camera components, asked for their streams from their first
    /// heartbeat (S39).
    asks: BTreeMap<Key, Ask>,
    pub(crate) sources: BTreeMap<Key, Source>,
    changed: bool,
    /// What a camera said of itself changed (S405).
    pub(crate) control_changed: bool,
    /// Photos described and not yet told.
    pub(crate) images: Vec<(Key, u8, crate::camera_control::Image)>,
}

impl Cameras {
    /// A camera's heartbeat.
    pub fn heard(&mut self, key: Key) {
        self.asks.entry(key).or_default();
    }

    pub fn answered(&mut self, key: &Key) {
        if let Some(ask) = self.asks.get_mut(key) {
            ask.answered = true;
        }
        if let Some(ask) = self.sources.get_mut(key).and_then(|s| s.streams.as_mut()) {
            ask.answered = true;
        }
    }

    /// The cameras to ask now, among those whose vehicle is known.
    pub fn due(&mut self, now_us: u64, known: impl Fn(&Key) -> bool) -> Vec<Key> {
        let mut out = Vec::new();
        for (key, ask) in &mut self.asks {
            if known(key) && ask.take(now_us, ATTEMPTS).is_some() {
                out.push(key.clone());
            }
        }
        out
    }

    /// A heartbeat of a camera component, or of an autopilot, which may
    /// speak for cameras. One back after it was gone is identified again
    /// from the beginning.
    pub fn heartbeat(&mut self, key: &Key, autopilot: bool, now_us: u64) {
        let Some(source) = self.sources.get_mut(key) else {
            self.sources.insert(
                key.clone(),
                Source {
                    autopilot,
                    heartbeat_us: now_us,
                    ..Source::default()
                },
            );
            // A camera component is listed before it says what it is.
            self.changed |= !autopilot;
            return;
        };
        source.heartbeat_us = source.heartbeat_us.max(now_us);
        if source.gone || source.autopilot != autopilot {
            *source = Source {
                autopilot,
                heartbeat_us: source.heartbeat_us,
                cameras: if autopilot == source.autopilot {
                    // Kept in the list, as cameras not yet identified.
                    source
                        .cameras
                        .keys()
                        .map(|device| (*device, Camera::default()))
                        .collect()
                } else {
                    BTreeMap::new()
                },
                ..Source::default()
            };
            if let Some(ask) = self.asks.get_mut(key) {
                *ask = Ask::default();
            }
            self.changed = true;
        }
    }

    /// Marks the cameras gone whose heartbeats, or whose autopilot's,
    /// stopped `after_us` ago, except on a link that is paused.
    pub fn silent(&mut self, now_us: u64, after_us: u64, paused: impl Fn(&str) -> bool) {
        for (key, source) in &mut self.sources {
            if !source.gone
                && now_us.saturating_sub(source.heartbeat_us) > after_us
                && !paused(&key.0)
            {
                source.gone = true;
                self.changed = true;
            }
        }
    }

    /// A link runs again after a pause: what was heard on it counts as
    /// heard now.
    pub fn resumed(&mut self, link_id: &str, now_us: u64) {
        for (key, source) in &mut self.sources {
            if key.0 == link_id && !source.gone {
                source.heartbeat_us = now_us;
            }
        }
    }

    /// A `CAMERA_INFORMATION` from a camera component or an autopilot.
    pub fn information(&mut self, key: &Key, information: Information) {
        let relayed = self.relayed(key, &information);
        let Some(source) = self.sources.get_mut(key) else {
            return;
        };
        source.information.answered = true;
        let device = if source.autopilot {
            match information.camera_device_id {
                id @ 1..=AUTOPILOT_CAMERAS => id,
                // ArduPilot 4.7 and earlier leave the device id out and
                // number their cameras from 0 in the lens id.
                _ => information.lens_id.min(AUTOPILOT_CAMERAS - 1) + 1,
            }
        } else {
            0
        };
        if relayed {
            // The camera's own answer, passed on by its autopilot: the
            // camera is listed once, as itself.
            self.changed |= source.cameras.remove(&device).is_some();
            return;
        }
        if information.flags & HAS_VIDEO_STREAM != 0 && source.streams.is_none() {
            source.streams = Some(match self.asks.get(key) {
                // A camera component is asked from its first heartbeat:
                // those are its first three asks, and only the superseded
                // command is left to try.
                Some(first) => Ask {
                    attempts: ATTEMPTS,
                    ..*first
                },
                None => Ask::default(),
            });
        }
        let camera = source.cameras.entry(device).or_default();
        if camera.information.as_ref() != Some(&information) {
            camera.information = Some(information);
            self.changed = true;
        }
        if !source.autopilot {
            self.fold_relayed(key);
        }
    }

    /// Whether an autopilot's answer is that of a camera component of the
    /// same system, which it passes on as ArduPilot does for a MAVLink
    /// camera it drives (`CAM1_TYPE` 6).
    fn relayed(&self, key: &Key, information: &Information) -> bool {
        self.sources.get(key).is_some_and(|source| source.autopilot)
            && information.camera_device_id == 0
            && self.sources.iter().any(|(other, source)| {
                !source.autopilot
                    && (&other.0, &other.1, other.2) == (&key.0, &key.1, key.2)
                    && source
                        .cameras
                        .get(&0)
                        .and_then(|camera| camera.information.as_ref())
                        .is_some_and(|own| own.names(information))
            })
    }

    /// Removes an autopilot's cameras that are the camera component at
    /// `key`, heard from the autopilot before the camera said what it is.
    fn fold_relayed(&mut self, key: &Key) {
        let Some(own) = self
            .sources
            .get(key)
            .and_then(|source| source.cameras.get(&0))
            .and_then(|camera| camera.information.clone())
        else {
            return;
        };
        for (other, source) in &mut self.sources {
            if !source.autopilot || (&other.0, &other.1, other.2) != (&key.0, &key.1, key.2) {
                continue;
            }
            let before = source.cameras.len();
            source.cameras.retain(|_, camera| {
                camera
                    .information
                    .as_ref()
                    .is_none_or(|theirs| theirs.camera_device_id != 0 || !own.names(theirs))
            });
            self.changed |= source.cameras.len() != before;
        }
    }

    /// The camera a stream or its status belongs to: the one its device id
    /// names, else the source's only camera.
    fn camera_of(source: &mut Source, device_id: u8) -> Option<&mut Camera> {
        if !source.autopilot {
            return Some(source.cameras.entry(0).or_default());
        }
        if (1..=AUTOPILOT_CAMERAS).contains(&device_id) {
            return source.cameras.get_mut(&device_id);
        }
        if source.cameras.len() == 1 {
            return source.cameras.values_mut().next();
        }
        None
    }

    /// A `VIDEO_STREAM_INFORMATION`: whether it names a stream. ArduPilot
    /// answers for a camera with none with every field 0, which ends the
    /// asking and names nothing.
    pub fn stream(&mut self, key: &Key, stream: Stream) -> bool {
        self.answered(key);
        if stream.uri.trim().is_empty() {
            return false;
        }
        let Some(source) = self.sources.get_mut(key) else {
            return true;
        };
        let Some(camera) = Self::camera_of(source, stream.camera_device_id) else {
            return true;
        };
        if camera.streams.get(&stream.stream_id) != Some(&stream) {
            camera.streams.insert(stream.stream_id, stream);
            self.changed = true;
        }
        true
    }

    /// A `VIDEO_STREAM_STATUS` for a stream already named.
    pub fn stream_status(&mut self, key: &Key, status: StreamStatus) {
        if let Some(source) = self.sources.get_mut(key) {
            source.watch.stream_said();
        }
        let Some(stream) = self
            .sources
            .get_mut(key)
            .and_then(|source| Self::camera_of(source, status.camera_device_id))
            .and_then(|camera| camera.streams.get_mut(&status.stream_id))
        else {
            return;
        };
        let now = Stream {
            flags: status.flags,
            frame_rate_hz: status.frame_rate_hz,
            width: status.width,
            height: status.height,
            bitrate: status.bitrate,
            rotation: status.rotation,
            hfov: status.hfov,
            ..stream.clone()
        };
        if *stream != now {
            *stream = now;
            self.changed = true;
        }
    }

    /// What to ask now, of those whose vehicle is known and heard.
    pub fn requests(&mut self, now_us: u64, known: impl Fn(&Key) -> bool) -> Vec<(Key, Request)> {
        let mut out: Vec<(Key, Request)> = self
            .due(now_us, &known)
            .into_iter()
            .map(|key| (key, Request::Message(VIDEO_STREAM_INFORMATION)))
            .collect();
        for (key, source) in &mut self.sources {
            if source.gone || !known(key) {
                continue;
            }
            let asked = |ask: &mut Ask, message: u32, superseded: u16| {
                ask.take(now_us, 2 * ATTEMPTS).map(|attempt| {
                    if attempt <= ATTEMPTS {
                        Request::Message(message)
                    } else {
                        Request::Superseded(superseded)
                    }
                })
            };
            if let Some(request) = asked(
                &mut source.information,
                CAMERA_INFORMATION,
                REQUEST_CAMERA_INFORMATION,
            ) {
                // A camera component that never says what it is stays
                // listed as one not identified.
                out.push((key.clone(), request));
            }
            // A camera component's first asks for its streams are those of
            // its first heartbeat, made above.
            let first = self.asks.get(key);
            if let (Some(ask), Some(first)) = (&mut source.streams, first) {
                ask.last_us = ask.last_us.max(first.last_us);
            }
            if let Some(ask) = &mut source.streams
                && first.is_none_or(|first| first.attempts >= ATTEMPTS)
                && let Some(request) = asked(
                    ask,
                    VIDEO_STREAM_INFORMATION,
                    REQUEST_VIDEO_STREAM_INFORMATION,
                )
            {
                out.push((key.clone(), request));
            }
            let named = source.cameras.values().any(|c| !c.streams.is_empty());
            if named && !source.status_asked {
                source.status_asked = true;
                out.push((key.clone(), Request::Message(VIDEO_STREAM_STATUS)));
            }
        }
        out
    }

    /// Whether a camera or an autopilot is still heard, so that time must
    /// go on for it to be found gone.
    #[must_use]
    pub fn any_heard(&self) -> bool {
        self.sources.values().any(|source| !source.gone)
    }

    /// Whether the cameras changed since this was last asked.
    pub fn take_changed(&mut self) -> bool {
        // What each camera said of itself is told again with it (S405).
        self.control_changed |= self.changed;
        std::mem::take(&mut self.changed)
    }

    /// The cameras are to be told again, such as when a vehicle's id
    /// changed.
    pub fn touch(&mut self) {
        self.changed = true;
    }

    /// Every camera known: where it was heard, its device id (0 for a
    /// camera component's own), whether it is heard, what it said and its
    /// streams.
    pub fn listed(
        &self,
    ) -> impl Iterator<Item = (&Key, u8, bool, Option<&Information>, Vec<Stream>)> {
        self.sources.iter().flat_map(|(key, source)| {
            let own = (!source.autopilot && source.cameras.is_empty())
                .then(|| (key, 0, !source.gone, None, Vec::new()));
            own.into_iter()
                .chain(source.cameras.iter().map(move |(device, camera)| {
                    (
                        key,
                        *device,
                        !source.gone,
                        camera.information.as_ref(),
                        camera.streams.values().cloned().collect(),
                    )
                }))
        })
    }

    /// Forgets what was heard at the addresses that match.
    pub fn forget(&mut self, gone: impl Fn(&Key) -> bool) {
        self.asks.retain(|key, _| !gone(key));
        let before = self.sources.len();
        self.sources.retain(|key, _| !gone(key));
        self.changed |= self.sources.len() != before;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_camera_is_asked_when_its_vehicle_is_known_and_again_until_it_answers() {
        let mut cameras = Cameras::default();
        let key: Key = ("l".into(), "p".into(), 1, 100);
        cameras.heard(key.clone());
        assert!(cameras.due(0, |_| false).is_empty(), "no vehicle yet");
        assert_eq!(cameras.due(0, |_| true), std::slice::from_ref(&key));
        assert!(cameras.due(1_000_000, |_| true).is_empty(), "too soon");
        assert_eq!(cameras.due(3_000_000, |_| true).len(), 1);
        assert_eq!(cameras.due(6_000_000, |_| true).len(), 1);
        assert!(
            cameras.due(9_000_000, |_| true).is_empty(),
            "three times at most"
        );
        let mut answered = Cameras::default();
        answered.heard(key.clone());
        answered.answered(&key);
        assert!(answered.due(0, |_| true).is_empty());
    }

    const S: u64 = 1_000_000;

    fn camera() -> Key {
        ("l".into(), "p".into(), 1, 100)
    }

    fn autopilot() -> Key {
        ("l".into(), "p".into(), 1, 1)
    }

    fn information(vendor: &str, flags: u32) -> Information {
        Information {
            vendor: vendor.into(),
            model: "Model".into(),
            firmware_version: 1,
            flags,
            ..Information::default()
        }
    }

    fn stream(uri: &str) -> Stream {
        Stream {
            stream_id: 1,
            uri: uri.into(),
            ..Stream::default()
        }
    }

    /// The asks of one kind in what is due at each second up to `until`.
    fn asked(
        cameras: &mut Cameras,
        key: &Key,
        until: u64,
        of: impl Fn(Request) -> bool,
    ) -> Vec<(u64, Request)> {
        let mut out = Vec::new();
        for second in 0..=until {
            for (to, request) in cameras.requests(second * S, |_| true) {
                if to == *key && of(request) {
                    out.push((second, request));
                }
            }
        }
        out
    }

    fn about_information(request: Request) -> bool {
        matches!(
            request,
            Request::Message(CAMERA_INFORMATION) | Request::Superseded(REQUEST_CAMERA_INFORMATION)
        )
    }

    fn about_streams(request: Request) -> bool {
        matches!(
            request,
            Request::Message(VIDEO_STREAM_INFORMATION)
                | Request::Superseded(REQUEST_VIDEO_STREAM_INFORMATION)
        )
    }

    #[test]
    fn a_camera_and_an_autopilot_are_asked_three_times_then_with_the_superseded_command() {
        for key in [camera(), autopilot()] {
            let mut cameras = Cameras::default();
            cameras.heartbeat(&key, key.3 == 1, 0);
            assert!(cameras.requests(0, |_| false).is_empty(), "no vehicle yet");
            assert_eq!(
                asked(&mut cameras, &key, 60, about_information),
                [
                    (0, Request::Message(CAMERA_INFORMATION)),
                    (3, Request::Message(CAMERA_INFORMATION)),
                    (6, Request::Message(CAMERA_INFORMATION)),
                    (9, Request::Superseded(REQUEST_CAMERA_INFORMATION)),
                    (12, Request::Superseded(REQUEST_CAMERA_INFORMATION)),
                    (15, Request::Superseded(REQUEST_CAMERA_INFORMATION)),
                ]
            );
        }
    }

    #[test]
    fn an_answer_ends_the_asking() {
        let mut cameras = Cameras::default();
        cameras.heartbeat(&camera(), false, 0);
        assert_eq!(cameras.requests(0, |_| true).len(), 1);
        cameras.information(&camera(), information("Acme", 0));
        assert!(asked(&mut cameras, &camera(), 60, |_| true).is_empty());
    }

    #[test]
    fn a_camera_component_is_listed_before_it_says_what_it_is_and_an_autopilot_is_not() {
        let mut cameras = Cameras::default();
        cameras.heartbeat(&autopilot(), true, 0);
        assert!(!cameras.take_changed());
        assert_eq!(cameras.listed().count(), 0);
        cameras.heartbeat(&camera(), false, 0);
        assert!(cameras.take_changed());
        let listed: Vec<_> = cameras.listed().collect();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            (listed[0].0, listed[0].1, listed[0].2),
            (&camera(), 0, true)
        );
        assert!(listed[0].3.is_none());
        cameras.information(&camera(), information("Acme", 3));
        assert!(cameras.take_changed());
        let listed: Vec<_> = cameras.listed().collect();
        assert_eq!(listed.len(), 1, "the same camera, now identified");
        assert_eq!(listed[0].3.map(|i| i.vendor.as_str()), Some("Acme"));
        cameras.information(&camera(), information("Acme", 3));
        assert!(!cameras.take_changed(), "said again, nothing new");
    }

    #[test]
    fn an_autopilot_speaks_for_its_cameras_by_device_id() {
        let mut cameras = Cameras::default();
        cameras.heartbeat(&autopilot(), true, 0);
        for device in [2, 1] {
            cameras.information(
                &autopilot(),
                Information {
                    camera_device_id: device,
                    ..information("", 2)
                },
            );
        }
        let devices: Vec<u8> = cameras.listed().map(|c| c.1).collect();
        assert_eq!(devices, [1, 2]);
        // Device ids above 6 are no camera's.
        cameras.information(
            &autopilot(),
            Information {
                camera_device_id: 9,
                lens_id: 2,
                ..information("", 2)
            },
        );
        let devices: Vec<u8> = cameras.listed().map(|c| c.1).collect();
        assert_eq!(devices, [1, 2, 3], "numbered by its lens id instead");
    }

    #[test]
    fn ardupilot_before_device_ids_numbers_its_cameras_by_lens_id() {
        // ArduPilot 4.7.2: no camera_device_id, lens_id the instance from 0.
        let mut cameras = Cameras::default();
        cameras.heartbeat(&autopilot(), true, 0);
        for instance in [0, 1] {
            cameras.information(
                &autopilot(),
                Information {
                    lens_id: instance,
                    flags: 2,
                    ..Information::default()
                },
            );
        }
        let devices: Vec<u8> = cameras.listed().map(|c| c.1).collect();
        assert_eq!(devices, [1, 2]);
    }

    #[test]
    fn a_camera_its_autopilot_passes_on_is_listed_once_whichever_is_heard_first() {
        let own = information("Acme", 3);
        // The camera first.
        let mut cameras = Cameras::default();
        cameras.heartbeat(&autopilot(), true, 0);
        cameras.heartbeat(&camera(), false, 0);
        cameras.information(&camera(), own.clone());
        cameras.information(&autopilot(), own.clone());
        let listed: Vec<_> = cameras.listed().map(|c| (c.0.3, c.1)).collect();
        assert_eq!(listed, [(100, 0)]);
        // The autopilot first.
        let mut cameras = Cameras::default();
        cameras.heartbeat(&autopilot(), true, 0);
        cameras.heartbeat(&camera(), false, 0);
        cameras.information(&autopilot(), own.clone());
        assert_eq!(cameras.listed().count(), 2, "not yet known to be the same");
        cameras.information(&camera(), own.clone());
        let listed: Vec<_> = cameras.listed().map(|c| (c.0.3, c.1)).collect();
        assert_eq!(listed, [(100, 0)]);
        // Another vehicle's camera of the same make is another camera.
        let other: Key = ("l".into(), "p".into(), 2, 1);
        cameras.heartbeat(&other, true, 0);
        cameras.information(&other, own.clone());
        assert_eq!(cameras.listed().count(), 2);
        // And one the autopilot numbers is its own.
        cameras.information(
            &autopilot(),
            Information {
                camera_device_id: 1,
                ..own
            },
        );
        assert_eq!(cameras.listed().count(), 3);
    }

    #[test]
    fn a_camera_with_streams_is_asked_for_them_and_an_empty_answer_names_none() {
        let mut cameras = Cameras::default();
        cameras.heartbeat(&autopilot(), true, 0);
        cameras.requests(0, |_| true);
        cameras.information(
            &autopilot(),
            Information {
                camera_device_id: 1,
                ..information("", HAS_VIDEO_STREAM)
            },
        );
        let mut unanswered = cameras.clone();
        assert_eq!(
            asked(&mut unanswered, &autopilot(), 60, about_streams),
            [
                (0, Request::Message(VIDEO_STREAM_INFORMATION)),
                (3, Request::Message(VIDEO_STREAM_INFORMATION)),
                (6, Request::Message(VIDEO_STREAM_INFORMATION)),
                (9, Request::Superseded(REQUEST_VIDEO_STREAM_INFORMATION)),
                (12, Request::Superseded(REQUEST_VIDEO_STREAM_INFORMATION)),
                (15, Request::Superseded(REQUEST_VIDEO_STREAM_INFORMATION)),
            ]
        );
        // ArduPilot's answer for a camera with no stream: every field 0.
        assert_eq!(cameras.requests(0, |_| true).len(), 1);
        assert!(!cameras.stream(&autopilot(), Stream::default()));
        assert!(asked(&mut cameras, &autopilot(), 60, about_streams).is_empty());
        assert!(cameras.listed().all(|c| c.4.is_empty()));
    }

    #[test]
    fn a_camera_component_asked_from_its_first_heartbeat_gets_the_superseded_command_after() {
        let mut cameras = Cameras::default();
        cameras.heartbeat(&camera(), false, 0);
        cameras.heard(camera());
        cameras.information(&camera(), information("Acme", HAS_VIDEO_STREAM));
        assert_eq!(
            asked(&mut cameras, &camera(), 60, about_streams),
            [
                (0, Request::Message(VIDEO_STREAM_INFORMATION)),
                (3, Request::Message(VIDEO_STREAM_INFORMATION)),
                (6, Request::Message(VIDEO_STREAM_INFORMATION)),
                (9, Request::Superseded(REQUEST_VIDEO_STREAM_INFORMATION)),
                (12, Request::Superseded(REQUEST_VIDEO_STREAM_INFORMATION)),
                (15, Request::Superseded(REQUEST_VIDEO_STREAM_INFORMATION)),
            ]
        );
    }

    #[test]
    fn a_stream_joins_its_camera_and_its_status_brings_it_up_to_date() {
        let mut cameras = Cameras::default();
        cameras.heartbeat(&camera(), false, 0);
        cameras.heard(camera());
        cameras.information(&camera(), information("Acme", HAS_VIDEO_STREAM));
        assert!(cameras.stream(&camera(), stream("rtsp://10.0.0.2/main")));
        assert!(cameras.take_changed());
        assert!(cameras.stream(&camera(), stream("rtsp://10.0.0.2/main")));
        assert!(!cameras.take_changed(), "said again, nothing new");
        let requests = cameras.requests(20 * S, |_| true);
        assert_eq!(
            requests,
            [(camera(), Request::Message(VIDEO_STREAM_STATUS))],
            "its streams are known, so only how they are is asked, once"
        );
        assert!(asked(&mut cameras, &camera(), 60, |_| true).is_empty());
        cameras.stream_status(
            &camera(),
            StreamStatus {
                stream_id: 1,
                flags: 1,
                frame_rate_hz: 30.0,
                width: 1280,
                height: 720,
                ..StreamStatus::default()
            },
        );
        assert!(cameras.take_changed());
        let listed: Vec<_> = cameras.listed().collect();
        let streams = &listed[0].4;
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].uri, "rtsp://10.0.0.2/main");
        assert_eq!(
            (streams[0].flags, streams[0].width, streams[0].height),
            (1, 1280, 720)
        );
        // A status for a stream never named is nobody's.
        cameras.stream_status(
            &camera(),
            StreamStatus {
                stream_id: 7,
                ..StreamStatus::default()
            },
        );
        assert!(!cameras.take_changed());
    }

    #[test]
    fn an_autopilots_stream_joins_the_camera_its_device_id_names() {
        let mut cameras = Cameras::default();
        cameras.heartbeat(&autopilot(), true, 0);
        for device in [1, 2] {
            cameras.information(
                &autopilot(),
                Information {
                    camera_device_id: device,
                    ..information("", HAS_VIDEO_STREAM)
                },
            );
        }
        assert!(cameras.stream(
            &autopilot(),
            Stream {
                camera_device_id: 2,
                ..stream("udp://0.0.0.0:5600")
            }
        ));
        // With no device id and two cameras it is neither's.
        assert!(cameras.stream(&autopilot(), stream("udp://0.0.0.0:5601")));
        let streams: Vec<(u8, usize)> = cameras.listed().map(|c| (c.1, c.4.len())).collect();
        assert_eq!(streams, [(1, 0), (2, 1)]);
    }

    #[test]
    fn a_camera_gone_is_kept_and_identified_again_when_it_returns() {
        let mut cameras = Cameras::default();
        cameras.heartbeat(&camera(), false, 0);
        cameras.heard(camera());
        cameras.requests(0, |_| true);
        cameras.information(&camera(), information("Acme", HAS_VIDEO_STREAM));
        cameras.stream(&camera(), stream("rtsp://10.0.0.2/main"));
        cameras.take_changed();
        cameras.silent(15 * S, 15 * S, |_| false);
        assert!(!cameras.take_changed(), "not yet");
        cameras.silent(16 * S, 15 * S, |_| true);
        assert!(!cameras.take_changed(), "its link is paused");
        cameras.silent(16 * S, 15 * S, |_| false);
        assert!(cameras.take_changed());
        let listed: Vec<_> = cameras.listed().collect();
        assert_eq!(listed.len(), 1, "kept");
        assert!(!listed[0].2, "gone");
        assert_eq!(listed[0].3.map(|i| i.vendor.as_str()), Some("Acme"));
        assert!(
            cameras.requests(20 * S, |_| true).is_empty(),
            "nothing is asked of a camera that is gone"
        );
        // Back: from the beginning.
        cameras.heartbeat(&camera(), false, 30 * S);
        cameras.heard(camera());
        assert!(cameras.take_changed());
        let listed: Vec<_> = cameras.listed().collect();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].2, "heard");
        assert!(listed[0].3.is_none(), "not yet identified again");
        assert!(listed[0].4.is_empty());
        let requests = cameras.requests(30 * S, |_| true);
        assert!(requests.contains(&(camera(), Request::Message(CAMERA_INFORMATION))));
        assert!(requests.contains(&(camera(), Request::Message(VIDEO_STREAM_INFORMATION))));
        cameras.information(&camera(), information("Acme", HAS_VIDEO_STREAM));
        let listed: Vec<_> = cameras.listed().collect();
        assert_eq!(listed[0].3.map(|i| i.vendor.as_str()), Some("Acme"));
    }

    #[test]
    fn an_autopilots_cameras_go_with_its_heartbeats_and_return_with_them() {
        let mut cameras = Cameras::default();
        cameras.heartbeat(&autopilot(), true, 0);
        cameras.information(
            &autopilot(),
            Information {
                camera_device_id: 1,
                ..information("", 2)
            },
        );
        cameras.silent(16 * S, 15 * S, |_| false);
        let listed: Vec<_> = cameras.listed().map(|c| (c.1, c.2)).collect();
        assert_eq!(listed, [(1, false)]);
        cameras.heartbeat(&autopilot(), true, 20 * S);
        let listed: Vec<_> = cameras
            .listed()
            .map(|c| (c.1, c.2, c.3.is_some()))
            .collect();
        assert_eq!(listed, [(1, true, false)]);
        assert_eq!(
            cameras.requests(20 * S, |_| true),
            [(autopilot(), Request::Message(CAMERA_INFORMATION))]
        );
    }

    #[test]
    fn a_paused_link_resumed_counts_as_heard_and_a_removed_one_is_forgotten() {
        let mut cameras = Cameras::default();
        cameras.heartbeat(&camera(), false, 0);
        cameras.take_changed();
        cameras.resumed("l", 100 * S);
        cameras.silent(101 * S, 15 * S, |_| false);
        assert!(!cameras.take_changed());
        cameras.forget(|key| key.0 == "l");
        assert!(cameras.take_changed());
        assert_eq!(cameras.listed().count(), 0);
    }
}

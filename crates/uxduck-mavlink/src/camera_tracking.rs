//! What a camera tracks and the range of its thermal picture (ADR-0063 Y5):
//! the three messages a camera sends of them, read from their bytes, and
//! what UXDuck asks a camera for so that they come.
//!
//! The camera protocol has a ground station ask for these itself (guide,
//! "Camera tracking"): the tracking status at an interval once a tracking
//! command was accepted, stopped again when the camera tracks no more, and
//! the thermal range while a stream says its range can be asked for. A
//! camera's stream is read back after it accepted another source, since
//! nothing else says which picture it sends.

use crate::camera::{Cameras, Key, VIDEO_STREAM_STATUS};
use crate::camera_control::Read;
use crate::decode::Message;

/// `CAMERA_TRACKING_IMAGE_STATUS`.
pub const CAMERA_TRACKING_IMAGE_STATUS: u32 = 275;
/// `CAMERA_TRACKING_GEO_STATUS`.
pub const CAMERA_TRACKING_GEO_STATUS: u32 = 276;
/// `CAMERA_THERMAL_RANGE`.
pub const CAMERA_THERMAL_RANGE: u32 = 277;
/// `MAV_CMD_SET_CAMERA_SOURCE`.
pub const SET_CAMERA_SOURCE: u16 = 534;
/// `MAV_CMD_CAMERA_TRACK_POINT`.
pub const CAMERA_TRACK_POINT: u16 = 2004;
/// `MAV_CMD_CAMERA_TRACK_RECTANGLE`.
pub const CAMERA_TRACK_RECTANGLE: u16 = 2005;
/// `MAV_CMD_CAMERA_STOP_TRACKING`.
pub const CAMERA_STOP_TRACKING: u16 = 2010;

/// `CAMERA_TRACKING_STATUS_FLAGS`: tracking, and in error. MAVLink made
/// them a bitmask and added two after mavio's dialect was cut (4, moving
/// target indication, and 8, coasting), so the status is kept as its bits.
pub const TRACKING_ACTIVE: u8 = 1;
pub const TRACKING_ERROR: u8 = 2;

/// `CAMERA_CAP_FLAGS`: tracks a point or a rectangle, and says where
/// what it tracks is on the ground.
const TRACKS: u32 = 512 | 1024;
const HAS_TRACKING_GEO_STATUS: u32 = 2048;
/// `VIDEO_STREAM_STATUS_FLAGS_THERMAL_RANGE_ENABLED`.
const THERMAL_RANGE_ENABLED: u16 = 4;

/// Where the target is in the picture, five times a second.
const IMAGE_INTERVAL_US: u32 = 200_000;
/// Where it is on the ground, and the thermal range, once a second.
const SLOW_INTERVAL_US: u32 = 1_000_000;
const ATTEMPTS: u8 = 3;
const RETRY_US: u64 = 3_000_000;
/// A command the camera accepted is read back after this long.
const SETTLE_US: u64 = 300_000;

/// A `CAMERA_TRACKING_IMAGE_STATUS`: what a camera tracks in its picture.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ImageSaid {
    /// `CAMERA_TRACKING_STATUS_FLAGS`.
    pub status: u8,
    /// `CAMERA_TRACKING_MODE`: 0 none, 1 a point, 2 a rectangle.
    pub mode: u8,
    /// Across and down the picture from 0 to 1, and its radius as a share
    /// of the picture's width. None for what the camera left unknown.
    pub point: Option<(f32, f32)>,
    pub radius: Option<f32>,
    /// Left, top, right and bottom, each from 0 to 1.
    pub rectangle: Option<[f32; 4]>,
    pub camera_device_id: u8,
}

/// A `CAMERA_TRACKING_GEO_STATUS`: where what a camera tracks is on the
/// ground.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GeoSaid {
    /// `CAMERA_TRACKING_STATUS_FLAGS`.
    pub status: u8,
    /// Degrees, and metres above mean sea level.
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    pub altitude_m: Option<f32>,
    /// From the camera to what it tracks, in metres.
    pub distance_m: Option<f32>,
    pub camera_device_id: u8,
}

/// A `CAMERA_THERMAL_RANGE`: the coldest and the warmest in a thermal
/// picture, in degrees Celsius, and where in the picture each is.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RangeSaid {
    pub stream_id: u8,
    pub camera_device_id: u8,
    pub min_c: f32,
    pub max_c: f32,
    pub min_at: Option<(f32, f32)>,
    pub max_at: Option<(f32, f32)>,
}

/// One of the three messages from a payload's bytes, or none for any
/// other message. A MAVLink 2 payload leaves its trailing zeros out.
#[must_use]
pub fn read(message_id: u32, payload: &[u8]) -> Option<Message> {
    let mut bytes = [0u8; 50];
    let length = payload.len().min(bytes.len());
    bytes[..length].copy_from_slice(&payload[..length]);
    let float =
        |at: usize| f32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    let whole =
        |at: usize| i32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    let known = |value: f32| value.is_finite().then_some(value);
    let place = |at: usize| known(float(at)).zip(known(float(at + 4)));
    match message_id {
        CAMERA_TRACKING_IMAGE_STATUS => {
            let sides = [12, 16, 20, 24].map(float);
            Some(Message::CameraTrackingImage(ImageSaid {
                status: bytes[28],
                mode: bytes[29],
                point: place(0),
                radius: known(float(8)),
                rectangle: sides.iter().all(|side| side.is_finite()).then_some(sides),
                camera_device_id: bytes[31],
            }))
        }
        CAMERA_TRACKING_GEO_STATUS => Some(Message::CameraTrackingGeo(GeoSaid {
            status: bytes[48],
            latitude_deg: f64::from(whole(0)) / 1e7,
            longitude_deg: f64::from(whole(4)) / 1e7,
            altitude_m: known(float(8)),
            distance_m: known(float(36)),
            camera_device_id: bytes[49],
        })),
        CAMERA_THERMAL_RANGE => Some(Message::CameraThermalRange(RangeSaid {
            stream_id: bytes[28],
            camera_device_id: bytes[29],
            max_c: float(4),
            max_at: place(8),
            min_c: float(16),
            min_at: place(20),
        })),
        _ => None,
    }
}

/// `VIDEO_STREAM_STATUS_FLAGS` from the bytes of a
/// `VIDEO_STREAM_INFORMATION` or a `VIDEO_STREAM_STATUS`, where both
/// carry them: mavio's dialect drops the flags it does not know, and that
/// a thermal range can be asked for is one of them.
#[must_use]
pub fn stream_flags(payload: &[u8]) -> u16 {
    u16::from_le_bytes([
        payload.get(8).copied().unwrap_or(0),
        payload.get(9).copied().unwrap_or(0),
    ])
}

/// What to send a camera component or an autopilot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ask {
    /// `MAV_CMD_REQUEST_MESSAGE` for a message id.
    Message(u32),
    /// `MAV_CMD_SET_MESSAGE_INTERVAL`: an interval in microseconds, or
    /// none to stop the message.
    Every {
        message: u32,
        interval_us: Option<u32>,
    },
}

/// A message asked for at an interval while something is so, and stopped
/// when it is so no more. It is asked for again when nothing comes of it,
/// since the acknowledgement of an interval does not say which message it
/// was for.
#[derive(Clone, Copy, Debug, Default)]
struct Paced {
    on: bool,
    attempts: u8,
    asked_us: u64,
    heard_us: u64,
}

impl Paced {
    /// What to do now: true to ask for it, false to stop it.
    fn due(&mut self, wanted: bool, now_us: u64) -> Option<bool> {
        if !wanted {
            self.attempts = 0;
            return std::mem::take(&mut self.on).then_some(false);
        }
        let silent = now_us.saturating_sub(self.heard_us.max(self.asked_us)) >= RETRY_US;
        if self.on && (!silent || self.attempts >= ATTEMPTS) {
            return None;
        }
        self.on = true;
        self.attempts += 1;
        self.asked_us = now_us;
        Some(true)
    }

    fn heard(&mut self, now_us: u64) {
        self.attempts = 0;
        self.heard_us = now_us;
    }
}

/// What is asked of a camera component, or of an autopilot about its
/// cameras, so that its tracking and its thermal range are known.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Watch {
    /// A camera of its that tracks was asked what it tracks now.
    started: bool,
    /// Its tracking status, read once and after each tracking command it
    /// accepted.
    status: Read,
    /// Its stream's status, read after it accepted another source.
    stream: Read,
    /// It last said it tracks something.
    tracking: bool,
    statuses: Paced,
    range: Paced,
}

impl Watch {
    /// A command the camera accepted, whoever sent it: what it changes is
    /// read back shortly.
    pub(crate) fn accepted(&mut self, command: u16, now_us: u64) {
        match command {
            SET_CAMERA_SOURCE => self.stream.want(now_us + SETTLE_US),
            CAMERA_TRACK_POINT | CAMERA_TRACK_RECTANGLE | CAMERA_STOP_TRACKING => {
                self.status.want(now_us + SETTLE_US);
            }
            _ => {}
        }
    }

    /// A `VIDEO_STREAM_STATUS` came.
    pub(crate) fn stream_said(&mut self) {
        self.stream.heard(0, None);
    }
}

impl Cameras {
    /// A tracking status from a camera component or an autopilot: whether
    /// it tracks, so that its status is asked for while it does. Says
    /// whether that changed.
    pub fn tracking_said(&mut self, key: &Key, status: u8, image: bool, now_us: u64) -> bool {
        let Some(source) = self.sources.get_mut(key) else {
            return false;
        };
        let watch = &mut source.watch;
        if image {
            watch.status.heard(now_us, None);
        }
        let tracking = status & (TRACKING_ACTIVE | TRACKING_ERROR) != 0;
        watch.statuses.heard(now_us);
        std::mem::replace(&mut watch.tracking, tracking) != tracking
    }

    /// A `CAMERA_THERMAL_RANGE` came.
    pub fn thermal_range_said(&mut self, key: &Key, now_us: u64) {
        if let Some(source) = self.sources.get_mut(key) {
            source.watch.range.heard(now_us);
        }
    }

    /// What to send now so that tracking and thermal ranges are known,
    /// among the cameras whose vehicle is known and heard.
    pub fn watch_requests(&mut self, now_us: u64, known: impl Fn(&Key) -> bool) -> Vec<(Key, Ask)> {
        let mut out = Vec::new();
        for (key, source) in &mut self.sources {
            if source.gone || !known(key) {
                continue;
            }
            let flags = source
                .cameras
                .values()
                .filter_map(|camera| camera.information.as_ref())
                .fold(0, |all, said| all | said.flags);
            let ranged = source
                .cameras
                .values()
                .any(|camera| camera.stream_flags() & THERMAL_RANGE_ENABLED != 0);
            let watch = &mut source.watch;
            // It may be tracking for another station already.
            if flags & TRACKS != 0 && !watch.started {
                watch.started = true;
                watch.status.want(now_us);
            }
            if watch.status.take(now_us, ATTEMPTS).is_some() {
                out.push((key.clone(), Ask::Message(CAMERA_TRACKING_IMAGE_STATUS)));
            }
            if watch.stream.take(now_us, ATTEMPTS).is_some() {
                out.push((key.clone(), Ask::Message(VIDEO_STREAM_STATUS)));
            }
            if let Some(on) = watch.statuses.due(watch.tracking, now_us) {
                out.push((
                    key.clone(),
                    Ask::Every {
                        message: CAMERA_TRACKING_IMAGE_STATUS,
                        interval_us: on.then_some(IMAGE_INTERVAL_US),
                    },
                ));
                if flags & HAS_TRACKING_GEO_STATUS != 0 {
                    out.push((
                        key.clone(),
                        Ask::Every {
                            message: CAMERA_TRACKING_GEO_STATUS,
                            interval_us: on.then_some(SLOW_INTERVAL_US),
                        },
                    ));
                }
            }
            if let Some(on) = watch.range.due(ranged, now_us) {
                out.push((
                    key.clone(),
                    Ask::Every {
                        message: CAMERA_THERMAL_RANGE,
                        interval_us: on.then_some(SLOW_INTERVAL_US),
                    },
                ));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use mavio::dialects::ardupilotmega::enums::{
        CameraTrackingMode, CameraTrackingStatusFlags, CameraTrackingTargetData,
        VideoStreamStatusFlags,
    };
    use mavio::dialects::ardupilotmega::messages::{
        CameraThermalRange, CameraTrackingGeoStatus, CameraTrackingImageStatus, VideoStreamStatus,
    };
    use uxduck_core::domain::camera::{CameraAction, CameraSource};
    use uxduck_core::domain::command::Command;

    use super::*;
    use crate::decode::decode;
    use crate::payload::{Payload, Place};
    use crate::session::{CommandOrder, CommandOutcome, Config, Effect, Session};
    use crate::sim::FakeVehicle;

    const PEER: &str = "127.0.0.1:5760";
    const SECOND: u64 = 1_000_000;

    fn decoded(frame: &[u8]) -> Message {
        decode(frame).expect("a frame").message
    }

    #[test]
    fn the_three_messages_are_read_from_their_bytes() {
        let image = CameraTrackingImageStatus {
            tracking_status: CameraTrackingStatusFlags::Active,
            tracking_mode: CameraTrackingMode::Rectangle,
            target_data: CameraTrackingTargetData::IN_STATUS,
            point_x: f32::NAN,
            point_y: f32::NAN,
            radius: f32::NAN,
            rec_top_x: 0.25,
            rec_top_y: 0.5,
            rec_bottom_x: 0.75,
            rec_bottom_y: 1.0,
            camera_device_id: 2,
        };
        assert_eq!(
            decoded(&crate::sim::frame(1, 100, 0, &image)),
            Message::CameraTrackingImage(ImageSaid {
                status: 1,
                mode: 2,
                point: None,
                radius: None,
                rectangle: Some([0.25, 0.5, 0.75, 1.0]),
                camera_device_id: 2,
            })
        );
        let point = CameraTrackingImageStatus {
            tracking_mode: CameraTrackingMode::Point,
            point_x: 0.5,
            point_y: 0.25,
            radius: 0.0,
            rec_top_x: f32::NAN,
            rec_top_y: f32::NAN,
            rec_bottom_x: f32::NAN,
            rec_bottom_y: f32::NAN,
            camera_device_id: 0,
            ..image
        };
        let Message::CameraTrackingImage(said) = decoded(&crate::sim::frame(1, 100, 0, &point))
        else {
            panic!("a tracking status");
        };
        assert_eq!((said.point, said.radius), (Some((0.5, 0.25)), Some(0.0)));
        assert_eq!(said.rectangle, None);

        let geo = CameraTrackingGeoStatus {
            tracking_status: CameraTrackingStatusFlags::Active,
            lat: 473_977_000,
            lon: 85_456_000,
            alt: 488.0,
            dist: 120.5,
            h_acc: f32::NAN,
            camera_device_id: 3,
            ..CameraTrackingGeoStatus::default()
        };
        let Message::CameraTrackingGeo(said) = decoded(&crate::sim::frame(1, 100, 0, &geo)) else {
            panic!("a geo status");
        };
        assert_eq!(said.status, 1);
        assert!((said.latitude_deg - 47.3977).abs() < 1e-9);
        assert!((said.longitude_deg - 8.5456).abs() < 1e-9);
        assert_eq!(
            (said.altitude_m, said.distance_m),
            (Some(488.0), Some(120.5))
        );
        assert_eq!(said.camera_device_id, 3);

        let range = CameraThermalRange {
            time_boot_ms: 1000,
            stream_id: 1,
            camera_device_id: 0,
            max: 41.5,
            max_point_x: 0.25,
            max_point_y: 0.75,
            min: 12.0,
            min_point_x: f32::NAN,
            min_point_y: f32::NAN,
        };
        assert_eq!(
            decoded(&crate::sim::frame(1, 100, 0, &range)),
            Message::CameraThermalRange(RangeSaid {
                stream_id: 1,
                camera_device_id: 0,
                min_c: 12.0,
                max_c: 41.5,
                min_at: None,
                max_at: Some((0.25, 0.75)),
            })
        );
    }

    // MAVLink made CAMERA_TRACKING_STATUS_FLAGS a bitmask and added moving
    // target indication (4) and coasting (8) after mavio's dialect was
    // cut, and THERMAL_RANGE_ENABLED (4) to VIDEO_STREAM_STATUS_FLAGS.
    #[test]
    fn flags_newer_than_the_dialect_are_kept() {
        let mut payload = vec![0u8; 31];
        payload[0..4].copy_from_slice(&0.5f32.to_le_bytes());
        payload[4..8].copy_from_slice(&0.5f32.to_le_bytes());
        // Tracking and coasting, of a point.
        payload[28] = 1 | 8;
        payload[29] = 1;
        let frame = crate::custom::frame_from((1, 100, 0), 275, &payload, 126);
        let Message::CameraTrackingImage(said) = decoded(&frame) else {
            panic!("a tracking status");
        };
        assert_eq!((said.status, said.mode), (9, 1));
        assert_eq!(said.point, Some((0.5, 0.5)));

        let status = VideoStreamStatus {
            stream_id: 1,
            flags: VideoStreamStatusFlags::from_bits_retain(1 | 2 | 4),
            ..VideoStreamStatus::default()
        };
        let Message::VideoStreamStatus(said) = decoded(&crate::sim::frame(1, 100, 0, &status))
        else {
            panic!("a stream's status");
        };
        assert_eq!(said.flags, 7);
    }

    /// A session and a fake vehicle that carries the unit that tracks and
    /// has a thermal sensor, 50 m up.
    struct Rig {
        session: Session,
        vehicle: FakeVehicle,
        now_us: u64,
        /// Message ids from the vehicle that do not arrive.
        lost: Vec<u32>,
        /// Commands from UXDuck that do not arrive.
        unheard: Vec<u16>,
        sent: Vec<(u8, u16, [f32; 7])>,
        made: Vec<Effect>,
    }

    impl Rig {
        fn new() -> Self {
            let mut vehicle = FakeVehicle::new(1, 0x1234);
            let mut payload = Payload::new(1).with_unit(Some("udp://127.0.0.1:15600".into()));
            if let Some(camera) = &mut payload.camera {
                camera.thermal_sensor = true;
                camera.tracks = true;
            }
            payload.vehicle(
                [0.0; 3],
                Some(Place {
                    lat: 473_977_000,
                    lon: 85_456_000,
                    alt_mm: 538_000,
                    relative_alt_mm: 50_000,
                }),
            );
            vehicle.payload = Some(payload);
            let mut rig = Self {
                session: Session::new(Config::default()),
                vehicle,
                now_us: 0,
                lost: Vec::new(),
                unheard: Vec::new(),
                sent: Vec::new(),
                made: Vec::new(),
            };
            for _ in 0..3 {
                rig.second();
            }
            rig
        }

        fn converse(&mut self, effects: Vec<Effect>) {
            let mut pending = effects;
            for _ in 0..1000 {
                if pending.is_empty() {
                    break;
                }
                let mut next = Vec::new();
                for effect in pending {
                    let Effect::Send { bytes, .. } = effect else {
                        self.made.push(effect);
                        continue;
                    };
                    if let Some(command) = crate::payload::wire::command(&bytes) {
                        self.sent
                            .push((command.to.1, command.command, command.params));
                        if self.unheard.contains(&command.command) {
                            continue;
                        }
                    }
                    for reply in self.vehicle.answer(&bytes) {
                        next.extend(self.hear(&reply));
                    }
                }
                pending = next;
            }
        }

        fn hear(&mut self, frame: &[u8]) -> Vec<Effect> {
            let id = crate::framing::header(frame).map(|header| header.message_id);
            if id.is_some_and(|id| self.lost.contains(&id)) {
                return Vec::new();
            }
            self.session.ingest("l1", PEER, frame, self.now_us)
        }

        fn step(&mut self) {
            self.now_us += SECOND / 10;
            #[allow(clippy::cast_precision_loss, reason = "a test's few seconds")]
            let t_s = self.now_us as f64 / 1e6;
            let mut effects = Vec::new();
            if self.now_us.is_multiple_of(SECOND) {
                let beat = self.vehicle.heartbeat();
                effects.extend(self.hear(&beat));
            }
            for frame in self.vehicle.payload_step(t_s) {
                effects.extend(self.hear(&frame));
            }
            effects.extend(self.session.tick(self.now_us));
            self.converse(effects);
        }

        fn second(&mut self) {
            for _ in 0..10 {
                self.step();
            }
        }

        fn act(&mut self, receipt: &str, action: CameraAction) {
            let order = CommandOrder {
                receipt_id: receipt.into(),
                vehicle_id: self
                    .session
                    .vehicle_at("l1", PEER, 1)
                    .expect("the vehicle")
                    .to_owned(),
                command: Command::Camera {
                    component_id: Some(100),
                    device_id: None,
                    action,
                },
                force: false,
            };
            let effects = self.session.command(order, self.now_us);
            self.converse(effects);
        }

        fn done(&self, receipt: &str) -> Option<CommandOutcome> {
            self.made.iter().find_map(|effect| match effect {
                Effect::CommandDone {
                    receipt_id,
                    outcome,
                    ..
                } if receipt_id == receipt => Some(outcome.clone()),
                _ => None,
            })
        }

        /// The intervals asked for a message, in order: below zero stops
        /// it.
        fn intervals(&self, message: u32) -> Vec<f32> {
            #[allow(clippy::cast_precision_loss, reason = "a small message id")]
            self.sent
                .iter()
                .filter(|(_, command, params)| {
                    *command == 511 && (params[0] - message as f32).abs() < 0.5
                })
                .map(|(_, _, params)| params[1])
                .collect()
        }

        fn asked(&self, message: u32) -> usize {
            #[allow(clippy::cast_precision_loss, reason = "a small message id")]
            self.sent
                .iter()
                .filter(|(_, command, params)| {
                    *command == 512 && (params[0] - message as f32).abs() < 0.5
                })
                .count()
        }

        /// The latest value of a telemetry item, and how many came.
        fn item(&self, key: &str) -> (Option<f64>, usize) {
            let values: Vec<f64> = self
                .made
                .iter()
                .filter_map(|effect| match effect {
                    Effect::Telemetry(update) => Some(&update.items),
                    _ => None,
                })
                .flatten()
                .filter(|item| item.key == key)
                .filter_map(|item| item.value)
                .collect();
            (values.last().copied(), values.len())
        }

        /// The flags of the camera's stream as last listed.
        fn stream_flags(&self) -> u16 {
            self.made
                .iter()
                .rev()
                .find_map(|effect| match effect {
                    Effect::Camera(seen) => seen.streams.first().map(|stream| stream.flags),
                    _ => None,
                })
                .expect("the camera's stream was listed")
        }
    }

    // Guide, camera protocol, "Camera tracking": the ground station sends
    // the tracking command, sets the interval of the two status messages,
    // and stops them after MAV_CMD_CAMERA_STOP_TRACKING.
    #[test]
    fn a_camera_that_tracks_has_its_status_asked_for_while_it_does() {
        let mut rig = Rig::new();
        // Asked once what it tracks, as it may track for another already.
        assert_eq!(rig.asked(CAMERA_TRACKING_IMAGE_STATUS), 1);
        assert!(rig.intervals(CAMERA_TRACKING_IMAGE_STATUS).is_empty());
        assert_eq!(rig.item("camera.tracking"), (Some(0.0), 1));

        rig.act("r1", CameraAction::TrackPoint { x: 0.5, y: 0.9 });
        assert_eq!(rig.done("r1"), Some(CommandOutcome::Accepted));
        assert_eq!(
            rig.sent
                .iter()
                .find(|(_, command, _)| *command == CAMERA_TRACK_POINT)
                .map(|(component, _, params)| (*component, params[0], params[1], params[2])),
            Some((100, 0.5, 0.9, 0.0))
        );
        rig.second();
        // Read back, and found tracking: both statuses at their intervals.
        assert_eq!(rig.asked(CAMERA_TRACKING_IMAGE_STATUS), 2);
        assert_eq!(rig.intervals(CAMERA_TRACKING_IMAGE_STATUS), [200_000.0]);
        assert_eq!(rig.intervals(CAMERA_TRACKING_GEO_STATUS), [1_000_000.0]);
        let before = rig.item("camera.tracking").1;
        rig.second();
        rig.second();
        let (status, count) = rig.item("camera.tracking");
        assert_eq!(status, Some(1.0));
        assert!((9..=11).contains(&(count - before)), "{}", count - before);
        assert_eq!(rig.item("camera.tracking_mode").0, Some(1.0));
        // The unit's gimbal has turned to it: it is in the middle.
        let (x, y) = (
            rig.item("camera.tracking_x").0,
            rig.item("camera.tracking_y").0,
        );
        assert!((x.expect("x") - 0.5).abs() < 0.02 && (y.expect("y") - 0.5).abs() < 0.02);
        assert_eq!(rig.item("camera.target").0, Some(1.0));
        let north = rig
            .item("camera.target_latitude_deg")
            .0
            .expect("a latitude")
            - 47.3977;
        assert!(north > 0.001, "ahead of the vehicle, to the north: {north}");
        assert!(rig.item("camera.target_distance_m").0.expect("a distance") > 50.0);

        rig.act("r2", CameraAction::StopTracking);
        assert_eq!(rig.done("r2"), Some(CommandOutcome::Accepted));
        rig.second();
        assert_eq!(rig.item("camera.tracking").0, Some(0.0));
        assert_eq!(
            rig.intervals(CAMERA_TRACKING_IMAGE_STATUS),
            [200_000.0, -1.0]
        );
        assert_eq!(
            rig.intervals(CAMERA_TRACKING_GEO_STATUS),
            [1_000_000.0, -1.0]
        );
        let stopped = rig.item("camera.tracking").1;
        rig.second();
        assert_eq!(rig.item("camera.tracking").1, stopped, "no more is said");
    }

    #[test]
    fn a_box_goes_as_its_corners_and_comes_back_as_its_sides() {
        let mut rig = Rig::new();
        rig.act(
            "r1",
            CameraAction::TrackRectangle {
                left: 0.4,
                top: 0.8,
                right: 0.6,
                bottom: 1.0,
            },
        );
        assert_eq!(rig.done("r1"), Some(CommandOutcome::Accepted));
        let sent = rig
            .sent
            .iter()
            .find(|(_, command, _)| *command == CAMERA_TRACK_RECTANGLE)
            .expect("the command");
        assert_eq!(sent.2[..5], [0.4, 0.8, 0.6, 1.0, 0.0]);
        rig.second();
        rig.second();
        assert_eq!(rig.item("camera.tracking_mode").0, Some(2.0));
        let side = |name: &str| {
            rig.item(&format!("camera.tracking_{name}"))
                .0
                .expect("a side")
        };
        assert!((side("right") - side("left") - 0.2).abs() < 0.02);
        assert!((f64::midpoint(side("left"), side("right")) - 0.5).abs() < 0.02);
        assert_eq!(
            rig.item("camera.tracking_x").1,
            0,
            "no point is said of a box"
        );
    }

    // An interval's acknowledgement does not say which message it is for,
    // so one that was lost is found by the silence after it.
    #[test]
    fn a_status_that_does_not_come_is_asked_for_again() {
        let mut rig = Rig::new();
        rig.unheard = vec![511];
        rig.act("r1", CameraAction::TrackPoint { x: 0.5, y: 0.9 });
        rig.second();
        assert_eq!(rig.intervals(CAMERA_TRACKING_IMAGE_STATUS).len(), 1);
        for _ in 0..3 {
            rig.second();
        }
        assert_eq!(rig.intervals(CAMERA_TRACKING_IMAGE_STATUS).len(), 2);
        rig.unheard.clear();
        for _ in 0..3 {
            rig.second();
        }
        assert_eq!(rig.intervals(CAMERA_TRACKING_IMAGE_STATUS).len(), 3);
        let before = rig.item("camera.tracking").1;
        for _ in 0..4 {
            rig.second();
        }
        assert_eq!(rig.intervals(CAMERA_TRACKING_IMAGE_STATUS).len(), 3);
        assert!(rig.item("camera.tracking").1 - before >= 18);
    }

    // common.xml: VIDEO_STREAM_STATUS_FLAGS_THERMAL says a stream is
    // thermal, and CAMERA_THERMAL_RANGE is asked for with
    // MAV_CMD_SET_MESSAGE_INTERVAL while _THERMAL_RANGE_ENABLED is set.
    #[test]
    fn another_source_is_read_back_from_the_stream_and_its_range_asked_for() {
        let mut rig = Rig::new();
        assert_eq!(rig.stream_flags(), 1);
        assert!(rig.intervals(CAMERA_THERMAL_RANGE).is_empty());

        // The camera's own word of its stream is lost on the way.
        rig.lost = vec![270];
        rig.act(
            "r1",
            CameraAction::SetSource {
                source: CameraSource::Thermal,
            },
        );
        assert_eq!(rig.done("r1"), Some(CommandOutcome::Accepted));
        assert_eq!(
            rig.sent
                .iter()
                .find(|(_, command, _)| *command == SET_CAMERA_SOURCE)
                .map(|(component, _, params)| (*component, params[0], params[1], params[2])),
            Some((100, 0.0, 2.0, 0.0))
        );
        let asked = rig.asked(270);
        rig.second();
        assert_eq!(
            rig.stream_flags(),
            1,
            "not shown as thermal on the asking alone"
        );
        assert_eq!(rig.asked(270), asked + 1);
        rig.lost.clear();
        for _ in 0..3 {
            rig.second();
        }
        assert_eq!(rig.asked(270), asked + 2);
        assert_eq!(rig.stream_flags(), 1 | 2 | 4);
        rig.second();
        assert_eq!(rig.intervals(CAMERA_THERMAL_RANGE), [1_000_000.0]);
        rig.second();
        rig.second();
        assert_eq!(rig.item("camera.thermal_min_degc").0, Some(0.0));
        assert_eq!(rig.item("camera.thermal_max_degc").0, Some(100.0));
        assert_eq!(rig.item("camera.thermal_max_x").1, 0, "it said no place");

        rig.act(
            "r2",
            CameraAction::SetSource {
                source: CameraSource::Daylight,
            },
        );
        rig.second();
        assert_eq!(rig.stream_flags(), 1);
        assert_eq!(rig.intervals(CAMERA_THERMAL_RANGE), [1_000_000.0, -1.0]);
        let said = rig.item("camera.thermal_max_degc").1;
        rig.second();
        rig.second();
        assert_eq!(rig.item("camera.thermal_max_degc").1, said);
    }

    // A camera with no second sensor says so itself.
    #[test]
    fn a_camera_with_one_sensor_refuses_another_in_its_own_words() {
        let mut rig = Rig::new();
        if let Some(camera) = rig
            .vehicle
            .payload
            .as_mut()
            .and_then(|payload| payload.camera.as_mut())
        {
            camera.thermal_sensor = false;
            camera.tracks = false;
        }
        rig.act(
            "r1",
            CameraAction::SetSource {
                source: CameraSource::Thermal,
            },
        );
        assert!(
            matches!(rig.done("r1"), Some(CommandOutcome::Rejected(text)) if text == "Unsupported"),
            "{:?}",
            rig.done("r1")
        );
        rig.act("r2", CameraAction::TrackPoint { x: 0.5, y: 0.5 });
        assert!(matches!(rig.done("r2"), Some(CommandOutcome::Rejected(_))));
        rig.second();
        assert!(rig.intervals(CAMERA_TRACKING_IMAGE_STATUS).is_empty());
    }
}

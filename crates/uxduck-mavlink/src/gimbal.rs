//! Finding a vehicle's gimbals and saying who holds them (MAVLink gimbal
//! protocol, version 2, ADR-0063 Y6, S406). Every autopilot and every
//! other component heard is asked whether it manages a gimbal
//! (`GIMBAL_MANAGER_INFORMATION`). One that does is asked to keep saying who
//! is in control (`GIMBAL_MANAGER_STATUS`), and for where the gimbal points
//! ten times a second only while someone watches it.
//!
//! What ArduPilot 4.8 does, as S400 and S406 found in its simulator: its
//! manager is the autopilot, the gimbal is device 1, its limits are its
//! own defaults until it has found the device, its manager's status flags
//! never change, and the flags of the `GIMBAL_DEVICE_ATTITUDE_STATUS` it
//! sends itself are true. So the flags are read from that message, the
//! information is asked again, and the autopilot is never asked for
//! message 286, which it does not send.
//!
//! What PX4 1.17 does, as S413 read in its source and found in its
//! simulator: its manager is the autopilot too, and it names the gimbal
//! by the device's own component (154). It answers
//! `GIMBAL_MANAGER_INFORMATION` only once it has found the device, with
//! the device's capabilities and limits, and says who is in control every
//! two seconds. It moves the gimbal only for whoever is in primary
//! control, and reads the command that takes control, the flags of a
//! movement and the message for rates differently from ArduPilot
//! ([`for_px4`], [`px4_rates_frame`]).

use std::collections::BTreeMap;

use uxduck_core::domain::gimbal::{GimbalAction, GimbalHolder, GimbalLimits};

use crate::commands::Wire;

/// `GIMBAL_MANAGER_INFORMATION`.
pub const GIMBAL_MANAGER_INFORMATION: u32 = 280;
/// `GIMBAL_MANAGER_STATUS`.
pub const GIMBAL_MANAGER_STATUS: u32 = 281;
/// `GIMBAL_DEVICE_ATTITUDE_STATUS`.
pub const GIMBAL_DEVICE_ATTITUDE_STATUS: u32 = 285;
/// `MAV_CMD_DO_SET_ROI_LOCATION`.
pub const DO_SET_ROI_LOCATION: u16 = 195;
/// `MAV_CMD_DO_GIMBAL_MANAGER_PITCHYAW`.
pub const DO_GIMBAL_MANAGER_PITCHYAW: u16 = 1000;
/// `MAV_CMD_DO_GIMBAL_MANAGER_CONFIGURE`.
pub const DO_GIMBAL_MANAGER_CONFIGURE: u16 = 1001;

/// `GIMBAL_DEVICE_FLAGS`, which the manager's flags share.
const RETRACT: u16 = 1;
const NEUTRAL: u16 = 2;
const ROLL_LOCK: u16 = 4;
const PITCH_LOCK: u16 = 8;
const YAW_LOCK: u16 = 16;
const YAW_IN_VEHICLE_FRAME: u16 = 32;
const YAW_IN_EARTH_FRAME: u16 = 64;

/// `MAV_FRAME_GLOBAL`: degrees times 1e7 in a `COMMAND_INT`, and the
/// height above mean sea level.
const MAV_FRAME_GLOBAL: u8 = 0;

/// The MAVLink components of gimbal devices, by the number an autopilot
/// gives the gimbal it drives: `MAV_COMP_ID_GIMBAL`, then
/// `MAV_COMP_ID_GIMBAL2` to `6`. ArduPilot looks for its mount's MAVLink
/// gimbal there.
const DEVICE_COMPONENTS: [u8; 6] = [154, 171, 172, 173, 174, 175];

const ATTEMPTS: u8 = 3;
const RETRY_US: u64 = 3_000_000;
/// A manager is asked what it is once more this long after its first
/// answer: ArduPilot gives its own limits until it has found its device,
/// some seconds after it starts.
const SETTLE_US: u64 = 15_000_000;
/// And this often while its gimbal is watched.
const REFRESH_US: u64 = 30_000_000;
/// An answer older than this is asked for again when watching starts.
const STALE_US: u64 = 10_000_000;
/// A manager that has not said who is in control for this long is asked
/// to say it every second.
const STATUS_SILENT_US: u64 = 5_000_000;
const STATUS_INTERVAL_US: u32 = 1_000_000;
/// Where a gimbal points, ten times a second while it is watched.
pub const WATCHED_INTERVAL_US: u32 = 100_000;
/// And once a second otherwise, to spare a telemetry radio.
pub const IDLE_INTERVAL_US: u32 = 1_000_000;
/// How long the attitude messages are counted for to tell their rate.
const RATE_WINDOW_US: u64 = 2_000_000;
const RATE_RETRY_US: u64 = 5_000_000;
/// Slower than this while watched, or faster than [`IDLE_HZ`] while not,
/// and the rate is asked for.
const WATCHED_HZ: f32 = 5.0;
const IDLE_HZ: f32 = 2.5;

/// By (link, peer, system id, component id).
pub type Key = crate::camera::Key;

/// A `GIMBAL_MANAGER_INFORMATION`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Information {
    /// `GIMBAL_MANAGER_CAP_FLAGS`.
    pub cap_flags: u32,
    /// 1 to 6 for a gimbal the manager drives itself, or the gimbal
    /// device's MAVLink component.
    pub gimbal_device_id: u8,
    /// Roll, pitch and yaw, from and to, in radians. NaN where unknown.
    pub limits: [(f32, f32); 3],
}

/// A `GIMBAL_MANAGER_STATUS`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    pub gimbal_device_id: u8,
    /// System and component in primary and in secondary control. Zeros
    /// are no one.
    pub primary: (u8, u8),
    pub secondary: (u8, u8),
}

/// What a `GIMBAL_DEVICE_ATTITUDE_STATUS` says, for the gimbals.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Attitude {
    /// `GIMBAL_DEVICE_FLAGS`.
    pub flags: u16,
    /// w, x, y, z.
    pub q: [f32; 4],
    pub gimbal_device_id: u8,
}

impl Attitude {
    /// Pitch from the horizon and yaw, in degrees, and whether the yaw is
    /// from north and not from the vehicle's nose. None without a whole
    /// quaternion.
    fn angles(&self) -> Option<(f32, f32, bool)> {
        if self.q.iter().any(|c| !c.is_finite()) {
            return None;
        }
        let [w, x, y, z] = self.q.map(f64::from);
        let pitch = (2.0 * (w * y - z * x)).clamp(-1.0, 1.0).asin().to_degrees();
        let yaw = (2.0 * (w * z + x * y))
            .atan2(1.0 - 2.0 * (y * y + z * z))
            .to_degrees();
        // Neither frame said: from north only while it is held there, as
        // the telemetry reads it.
        let north = self.flags & YAW_IN_EARTH_FRAME != 0
            || (self.flags & YAW_IN_VEHICLE_FRAME == 0 && self.flags & YAW_LOCK != 0);
        #[allow(clippy::cast_possible_truncation, reason = "angles in degrees")]
        Some((pitch as f32, yaw as f32, north))
    }
}

/// The `gimbal_device_id` of a `GIMBAL_DEVICE_ATTITUDE_STATUS` frame: an
/// extension field, so 0 where the frame ends before it.
#[must_use]
pub fn attitude_device_id(frame: &[u8]) -> u8 {
    crate::framing::payload(frame)
        .filter(|_| frame.first() == Some(&0xFD))
        .and_then(|payload| payload.get(48).copied())
        .unwrap_or(0)
}

/// The `failure_flags` of a `GIMBAL_DEVICE_ATTITUDE_STATUS` frame
/// (`GIMBAL_DEVICE_ERROR_FLAGS`): what the device says is wrong with it.
/// MAVLink 2 leaves off a payload's last zeros, so bytes that are not
/// there are zero.
#[must_use]
pub fn attitude_failure_flags(frame: &[u8]) -> u32 {
    let mut bytes = [0u8; 4];
    if let Some(payload) = crate::framing::payload(frame) {
        for (byte, sent) in bytes.iter_mut().zip(payload.iter().skip(32)) {
            *byte = *sent;
        }
    }
    u32::from_le_bytes(bytes)
}

/// A gimbal of a vehicle as it is now known.
#[derive(Clone, Debug, PartialEq)]
pub struct Observation {
    pub vehicle_id: String,
    /// The manager's component.
    pub component_id: u8,
    /// What the manager calls the gimbal.
    pub device_id: u8,
    /// The gimbal device's own component, when it has one that is heard.
    pub device_component_id: Option<u8>,
    /// Whether its manager's heartbeats are heard.
    pub present: bool,
    pub cap_flags: u32,
    pub limits: GimbalLimits,
    /// None for no one, and before the manager has said.
    pub primary: Option<(u8, u8)>,
    pub secondary: Option<(u8, u8)>,
    pub holder: GimbalHolder,
    /// From the device's own flags, once it has said them.
    pub yaw_locked: Option<bool>,
    pub retracted: bool,
    pub neutral: bool,
    /// `GIMBAL_DEVICE_ERROR_FLAGS`: what the device says is wrong with it.
    pub failure_flags: u32,
}

/// What to send a component.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    /// `MAV_CMD_REQUEST_MESSAGE` for `GIMBAL_MANAGER_INFORMATION`.
    Information,
    /// `MAV_CMD_SET_MESSAGE_INTERVAL`: a message id and microseconds.
    Interval(u32, u32),
}

#[derive(Clone, Copy, Debug, Default)]
struct Ask {
    attempts: u8,
    last_us: u64,
    answered: bool,
}

impl Ask {
    /// Counts an ask if one is due, `every_us` after the last, and fewer
    /// than three were made.
    fn take(&mut self, now_us: u64, every_us: u64) -> bool {
        let waited = self.attempts == 0 || now_us.saturating_sub(self.last_us) >= every_us;
        if self.answered || self.attempts >= ATTEMPTS || !waited {
            return false;
        }
        self.attempts += 1;
        self.last_us = now_us;
        true
    }
}

/// A gimbal a manager answers for.
#[derive(Clone, Copy, Debug)]
struct Managed {
    information: Information,
    status: Option<Status>,
    status_us: u64,
    attitude: Option<Attitude>,
    /// What is wrong with the gimbal, as its manager's component says and
    /// as the device's own component says. An autopilot that manages a
    /// device says its own account beside the device's, so the two are
    /// kept apart and neither overwrites the other.
    failures: [u32; 2],
    /// Attitude messages from the manager's own component since
    /// `window_us`, and how many a second there were before it.
    window_us: u64,
    count: u32,
    hz: Option<f32>,
}

/// What a component's heartbeat says it is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
    /// A flight controller, which may drive gimbals with no MAVLink
    /// component of their own.
    Autopilot,
    /// A gimbal: a device an autopilot may drive, or its own manager.
    Gimbal,
    /// Anything else that is not a ground station.
    #[default]
    Other,
}

/// A component that may manage a gimbal: an autopilot, a gimbal, or any
/// other that is not a ground station.
#[derive(Clone, Debug, Default)]
struct Source {
    kind: Kind,
    heartbeat_us: u64,
    gone: bool,
    information: Ask,
    /// When it last said what it manages, and when it was last asked
    /// again after that.
    answered_us: u64,
    refreshed_us: u64,
    settled: bool,
    status: Ask,
    rate: Ask,
    watched: bool,
    /// By the device id its information gave.
    gimbals: BTreeMap<u8, Managed>,
}

/// Every component that may manage a gimbal, and the gimbals found.
#[derive(Clone, Debug, Default)]
pub struct Gimbals {
    sources: BTreeMap<Key, Source>,
    changed: bool,
}

impl Gimbals {
    /// A heartbeat of an autopilot or of another component of a vehicle.
    /// One back after it was gone is asked again from the beginning.
    pub fn heartbeat(&mut self, key: &Key, kind: Kind, now_us: u64) {
        let Some(source) = self.sources.get_mut(key) else {
            self.sources.insert(
                key.clone(),
                Source {
                    kind,
                    heartbeat_us: now_us,
                    ..Source::default()
                },
            );
            // A gimbal device heard names the device an autopilot drives.
            self.changed |= kind == Kind::Gimbal;
            return;
        };
        source.heartbeat_us = source.heartbeat_us.max(now_us);
        if source.gone || source.kind != kind {
            let kept = if source.kind == kind {
                std::mem::take(&mut source.gimbals)
            } else {
                BTreeMap::new()
            };
            *source = Source {
                kind,
                heartbeat_us: source.heartbeat_us,
                gimbals: kept
                    .into_iter()
                    .map(|(device, managed)| {
                        // Kept in the list, and its status read again.
                        let managed = Managed {
                            status: None,
                            attitude: None,
                            failures: [0; 2],
                            count: 0,
                            hz: None,
                            ..managed
                        };
                        (device, managed)
                    })
                    .collect(),
                ..Source::default()
            };
            self.changed = true;
        }
    }

    /// Marks the gimbals gone whose manager's heartbeats stopped
    /// `after_us` ago, except on a link that is paused.
    pub fn silent(&mut self, now_us: u64, after_us: u64, paused: impl Fn(&str) -> bool) {
        for (key, source) in &mut self.sources {
            if !source.gone
                && now_us.saturating_sub(source.heartbeat_us) > after_us
                && !paused(&key.0)
            {
                source.gone = true;
                self.changed |= source.kind == Kind::Gimbal || !source.gimbals.is_empty();
            }
        }
    }

    /// A link runs again after a pause: what was heard on it counts as
    /// heard now.
    pub fn resumed(&mut self, link_id: &str, now_us: u64) {
        for (key, source) in &mut self.sources {
            if key.0 == link_id && !source.gone {
                source.heartbeat_us = now_us;
                for managed in source.gimbals.values_mut() {
                    managed.status_us = now_us;
                    managed.window_us = now_us;
                    managed.count = 0;
                }
            }
        }
    }

    /// A `GIMBAL_MANAGER_INFORMATION` from the component at `key`.
    pub fn information(&mut self, key: &Key, information: Information, now_us: u64) {
        let Some(source) = self.sources.get_mut(key) else {
            return;
        };
        source.information.answered = true;
        source.answered_us = now_us;
        if let Some(managed) = source.gimbals.get_mut(&information.gimbal_device_id) {
            // NaN is not equal to itself: compared as bits.
            let bits = |i: &Information| {
                (
                    i.cap_flags,
                    i.limits.map(|(low, high)| (low.to_bits(), high.to_bits())),
                )
            };
            self.changed |= bits(&managed.information) != bits(&information);
            managed.information = information;
            return;
        }
        source.gimbals.insert(
            information.gimbal_device_id,
            Managed {
                information,
                status: None,
                status_us: now_us,
                attitude: None,
                failures: [0; 2],
                window_us: now_us,
                count: 0,
                hz: None,
            },
        );
        self.changed = true;
    }

    /// A `GIMBAL_MANAGER_STATUS` from the component at `key`. One for a
    /// gimbal the manager has not described yet is left.
    pub fn status(&mut self, key: &Key, status: Status, now_us: u64) {
        let Some(source) = self.sources.get_mut(key) else {
            return;
        };
        let Some(managed) = source.gimbals.get_mut(&status.gimbal_device_id) else {
            return;
        };
        managed.status_us = now_us;
        source.status = Ask::default();
        if managed.status != Some(status) {
            managed.status = Some(status);
            self.changed = true;
        }
    }

    /// A `GIMBAL_DEVICE_ATTITUDE_STATUS` from the component at `key`:
    /// the manager's own account of its gimbal, or a gimbal device's that
    /// a manager elsewhere names by its component.
    pub fn attitude(&mut self, key: &Key, attitude: Attitude) {
        let flags = |attitude: &Attitude| attitude.flags & (RETRACT | NEUTRAL | YAW_LOCK);
        let take = |managed: &mut Managed, own: bool| {
            let changed = managed.attitude.as_ref().map(flags) != Some(flags(&attitude));
            managed.attitude = Some(attitude);
            if own {
                managed.count += 1;
            }
            changed
        };
        if let Some(source) = self.sources.get_mut(key) {
            let only = source.gimbals.len() == 1 && attitude.gimbal_device_id == 0;
            let own = if only {
                source.gimbals.values_mut().next()
            } else {
                source.gimbals.get_mut(&attitude.gimbal_device_id)
            };
            if let Some(managed) = own {
                self.changed |= take(managed, true);
                return;
            }
        }
        // A device with a component of its own, managed from another.
        for (other, source) in &mut self.sources {
            if (&other.0, &other.1, other.2) != (&key.0, &key.1, key.2) || other.3 == key.3 {
                continue;
            }
            if let Some(managed) = source.gimbals.get_mut(&key.3)
                && key.3 > 6
            {
                self.changed |= take(managed, false);
            }
        }
    }

    /// The `failure_flags` of a `GIMBAL_DEVICE_ATTITUDE_STATUS` from the
    /// component at `key`, for the gimbal it calls `gimbal_device_id`:
    /// found as [`Self::attitude`] finds whose attitude it is.
    pub fn failures(&mut self, key: &Key, gimbal_device_id: u8, flags: u32) {
        if let Some(source) = self.sources.get_mut(key) {
            let own = if source.gimbals.len() == 1 && gimbal_device_id == 0 {
                source.gimbals.values_mut().next()
            } else {
                source.gimbals.get_mut(&gimbal_device_id)
            };
            if let Some(managed) = own {
                self.changed |= managed.failures[0] != flags;
                managed.failures[0] = flags;
                return;
            }
        }
        // A device with a component of its own, managed from another: one
        // its manager names by that component, or the mount of an
        // autopilot whose device that component is ([`Self::listed`]).
        let mount = DEVICE_COMPONENTS
            .iter()
            .position(|component| *component == key.3)
            .and_then(|index| u8::try_from(index + 1).ok());
        for (other, source) in &mut self.sources {
            if (&other.0, &other.1, other.2) != (&key.0, &key.1, key.2) || other.3 == key.3 {
                continue;
            }
            let named = if key.3 > 6 && source.gimbals.contains_key(&key.3) {
                Some(key.3)
            } else {
                mount.filter(|_| source.kind == Kind::Autopilot)
            };
            if let Some(managed) = named.and_then(|device| source.gimbals.get_mut(&device)) {
                self.changed |= managed.failures[1] != flags;
                managed.failures[1] = flags;
            }
        }
    }

    /// What to ask now, of those whose vehicle is known and heard.
    /// `watched` says whether someone is looking at where a component's
    /// gimbals point.
    pub fn requests(
        &mut self,
        now_us: u64,
        known: impl Fn(&Key) -> bool,
        watched: impl Fn(&Key) -> bool,
    ) -> Vec<(Key, Request)> {
        let mut out = Vec::new();
        for (key, source) in &mut self.sources {
            if source.gone || !known(key) {
                continue;
            }
            if source.information.take(now_us, RETRY_US) {
                out.push((key.clone(), Request::Information));
            }
            if source.gimbals.is_empty() || !source.information.answered {
                continue;
            }
            let watching = watched(key);
            let began = watching && !source.watched;
            if watching != source.watched {
                source.watched = watching;
                source.rate = Ask::default();
            }
            // Its limits may have changed since it first answered.
            let since_answer = now_us.saturating_sub(source.answered_us);
            let since_asked = now_us.saturating_sub(source.refreshed_us.max(source.answered_us));
            let again = (!source.settled && since_answer >= SETTLE_US)
                || (began && since_answer >= STALE_US)
                || (watching && since_asked >= REFRESH_US);
            if again {
                source.settled = true;
                source.refreshed_us = now_us;
                out.push((key.clone(), Request::Information));
            }
            let mut silent = false;
            let mut rate = None;
            for managed in source.gimbals.values_mut() {
                silent |= now_us.saturating_sub(managed.status_us) >= STATUS_SILENT_US;
                let window = now_us.saturating_sub(managed.window_us);
                if window >= RATE_WINDOW_US {
                    #[allow(clippy::cast_precision_loss, reason = "a count over seconds")]
                    let hz = managed.count as f32 / (window as f32 / 1e6);
                    managed.hz = Some(hz);
                    managed.count = 0;
                    managed.window_us = now_us;
                }
                let hz = managed.hz.unwrap_or(0.0);
                if watching && hz < WATCHED_HZ {
                    rate = Some(WATCHED_INTERVAL_US);
                } else if !watching && hz > IDLE_HZ {
                    rate = rate.or(Some(IDLE_INTERVAL_US));
                }
            }
            if silent && source.status.take(now_us, STATUS_SILENT_US) {
                out.push((
                    key.clone(),
                    Request::Interval(GIMBAL_MANAGER_STATUS, STATUS_INTERVAL_US),
                ));
            }
            match rate {
                // As asked: a later change is asked for from the start.
                None => source.rate = Ask::default(),
                Some(interval_us) => {
                    if source.rate.take(now_us, RATE_RETRY_US) {
                        out.push((
                            key.clone(),
                            Request::Interval(GIMBAL_DEVICE_ATTITUDE_STATUS, interval_us),
                        ));
                    }
                }
            }
        }
        out
    }

    /// Whether a manager or a gimbal device is still heard, so that time
    /// must go on for it to be found gone.
    #[must_use]
    pub fn any_heard(&self) -> bool {
        self.sources.values().any(|source| {
            !source.gone && (source.kind == Kind::Gimbal || !source.gimbals.is_empty())
        })
    }

    /// Whether the gimbals changed since this was last asked.
    pub fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    /// The gimbals are to be told again, such as when a vehicle's id
    /// changed.
    pub fn touch(&mut self) {
        self.changed = true;
    }

    /// Every gimbal known: where its manager was heard, and what is known
    /// of it but its vehicle and who holds it, which the session knows.
    #[must_use]
    pub fn listed(&self) -> Vec<(Key, Listed)> {
        let mut out = Vec::new();
        for (key, source) in &self.sources {
            for (device, managed) in &source.gimbals {
                let device_component_id = if *device > 6 {
                    Some(*device)
                } else if source.kind != Kind::Autopilot {
                    // It manages itself.
                    Some(key.3)
                } else {
                    device
                        .checked_sub(1)
                        .and_then(|index| DEVICE_COMPONENTS.get(usize::from(index)))
                        .copied()
                        .filter(|component| {
                            self.sources
                                .get(&(key.0.clone(), key.1.clone(), key.2, *component))
                                .is_some_and(|heard| heard.kind == Kind::Gimbal && !heard.gone)
                        })
                };
                let degrees = |radians: f32| radians.is_finite().then(|| radians.to_degrees());
                let [roll, pitch, yaw] = managed.information.limits;
                let flags = managed.attitude.map(|attitude| attitude.flags);
                out.push((
                    key.clone(),
                    Listed {
                        device_id: *device,
                        device_component_id,
                        heard: !source.gone,
                        cap_flags: managed.information.cap_flags,
                        limits: GimbalLimits {
                            roll_min_deg: degrees(roll.0),
                            roll_max_deg: degrees(roll.1),
                            pitch_min_deg: degrees(pitch.0),
                            pitch_max_deg: degrees(pitch.1),
                            yaw_min_deg: degrees(yaw.0),
                            yaw_max_deg: degrees(yaw.1),
                        },
                        status: managed.status,
                        yaw_locked: flags.map(|flags| flags & YAW_LOCK != 0),
                        retracted: flags.is_some_and(|flags| flags & RETRACT != 0),
                        neutral: flags.is_some_and(|flags| flags & NEUTRAL != 0),
                        failure_flags: managed.failures[0] | managed.failures[1],
                    },
                ));
            }
        }
        out
    }

    /// Where the gimbal a manager calls `device_id` last said it points:
    /// pitch and yaw in degrees, and whether the yaw is from north. With
    /// device 0, the manager's only gimbal.
    #[must_use]
    pub fn pointing(&self, key: &Key, device_id: u8) -> Option<(f32, f32, bool)> {
        let source = self.sources.get(key)?;
        let managed = if device_id == 0 && source.gimbals.len() == 1 {
            source.gimbals.values().next()
        } else {
            source.gimbals.get(&device_id)
        }?;
        managed.attitude?.angles()
    }

    /// Whether the gimbal a manager calls `device_id` last said it holds
    /// its yaw from north. With device 0, the manager's only gimbal. None
    /// until it has said.
    #[must_use]
    pub fn yaw_locked(&self, key: &Key, device_id: u8) -> Option<bool> {
        let source = self.sources.get(key)?;
        let managed = if device_id == 0 && source.gimbals.len() == 1 {
            source.gimbals.values().next()
        } else {
            source.gimbals.get(&device_id)
        }?;
        Some(managed.attitude?.flags & YAW_LOCK != 0)
    }

    /// Forgets what was heard at the addresses that match.
    pub fn forget(&mut self, gone: impl Fn(&Key) -> bool) {
        let before = self.sources.len();
        self.sources.retain(|key, _| !gone(key));
        self.changed |= self.sources.len() != before;
    }
}

/// A gimbal as its manager and its device describe it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Listed {
    pub device_id: u8,
    pub device_component_id: Option<u8>,
    pub heard: bool,
    pub cap_flags: u32,
    pub limits: GimbalLimits,
    pub status: Option<Status>,
    pub yaw_locked: Option<bool>,
    pub retracted: bool,
    pub neutral: bool,
    pub failure_flags: u32,
}

/// The `MAV_CMD` of a gimbal's action and how it goes on the wire, for
/// the gimbal its manager calls `device_id` (0 for every gimbal it has).
/// Following the nose and holding north go as the pitch and yaw the
/// gimbal has now, which the session puts in ([`keep_pointing`]), since
/// ArduPilot refuses a `DO_GIMBAL_MANAGER_PITCHYAW` without both angles
/// (S406).
///
/// # Errors
/// When its numbers are not ones a gimbal can be given, or a place has
/// no height.
pub fn encode(action: &GimbalAction, device_id: u8) -> Result<(u16, Wire), String> {
    if let Some(why) = action.invalid() {
        return Err(why.into());
    }
    let device = f32::from(device_id);
    let configure = |seat: f32| {
        (
            DO_GIMBAL_MANAGER_CONFIGURE,
            // Primary control, and secondary control left as it is.
            Wire::Long([seat, seat, -1.0, -1.0, 0.0, 0.0, device]),
        )
    };
    let pitch_yaw = |pitch: f32, yaw: f32, flags: u16| {
        (
            DO_GIMBAL_MANAGER_PITCHYAW,
            // Angles and no rates.
            Wire::Long([
                pitch,
                yaw,
                f32::NAN,
                f32::NAN,
                f32::from(flags),
                0.0,
                device,
            ]),
        )
    };
    Ok(match *action {
        // -2: the sender. -3: the sender gives it up.
        GimbalAction::TakeControl => configure(-2.0),
        GimbalAction::ReleaseControl => configure(-3.0),
        GimbalAction::Point {
            pitch_deg,
            yaw_deg,
            lock,
        } => pitch_yaw(
            pitch_deg,
            within_half_a_turn(yaw_deg),
            if lock { YAW_LOCK } else { 0 },
        ),
        GimbalAction::Centre => pitch_yaw(0.0, 0.0, 0),
        GimbalAction::Follow => pitch_yaw(f32::NAN, f32::NAN, 0),
        GimbalAction::HoldNorth => pitch_yaw(f32::NAN, f32::NAN, YAW_LOCK),
        GimbalAction::LookAt {
            latitude_deg,
            longitude_deg,
            altitude_amsl_m,
        } => {
            let Some(altitude_m) = altitude_amsl_m else {
                return Err("a place to look at needs its height above sea level".into());
            };
            #[allow(clippy::cast_possible_truncation, reason = "within ±1.8e9")]
            let e7 = |degrees: f64| (degrees * 1e7).round() as i32;
            (
                DO_SET_ROI_LOCATION,
                Wire::Int {
                    frame: MAV_FRAME_GLOBAL,
                    params: [device, 0.0, 0.0, 0.0],
                    x: e7(latitude_deg),
                    y: e7(longitude_deg),
                    #[allow(clippy::cast_possible_truncation, reason = "a height in metres")]
                    z: altitude_m as f32,
                },
            )
        }
    })
}

/// Puts the pitch and yaw a gimbal has now into a command that changes
/// only what its yaw is held from: `pointing` is its pitch and yaw in
/// degrees and whether that yaw is from north, and `heading_deg` the
/// vehicle's own.
///
/// # Errors
/// When the gimbal has not said where it points, or its yaw has to be
/// turned between the two frames and the vehicle's heading is not known.
pub fn keep_pointing(
    action: &GimbalAction,
    wire: &mut Wire,
    pointing: Option<(f32, f32, bool)>,
    heading_deg: Option<f32>,
) -> Result<(), &'static str> {
    let north = match action {
        GimbalAction::Follow => false,
        GimbalAction::HoldNorth => true,
        _ => return Ok(()),
    };
    let Wire::Long(params) = wire else {
        return Ok(());
    };
    let (pitch, yaw, from_north) = pointing.ok_or("the gimbal has not said where it points yet")?;
    let yaw = if from_north == north {
        yaw
    } else {
        let heading = heading_deg.ok_or("the vehicle's heading is not known yet")?;
        if north { yaw + heading } else { yaw - heading }
    };
    params[0] = pitch;
    params[1] = within_half_a_turn(yaw);
    Ok(())
}

/// How many times a gimbal is told to stop turning (S407): a zero rate is
/// one message with no answer, and one lost would leave the gimbal
/// turning.
pub const STOP_SENDS: u8 = 3;
/// A vehicle heard within this is told again to stop its gimbal now. One
/// that is not is told when it is heard again.
pub const STOP_HEARD_US: u64 = 2_500_000;

/// A `GIMBAL_MANAGER_SET_PITCHYAW` that turns a gimbal at rates (S407):
/// pitch then yaw in radians a second, up and right positive, with no
/// angles, since ArduPilot ignores angles and rates sent together and
/// ignores `GIMBAL_MANAGER_SET_MANUAL_CONTROL` (S400). Both rates are
/// always given, since ArduPilot takes rates only as a pair. Zero rates
/// stop it. The yaw stays held from north or from the nose as the gimbal
/// last said it is, which is the only flag sent.
#[must_use]
pub fn rates_frame(
    (gcs_system, gcs_component, sequence): (u8, u8, u8),
    (target_system, target_component): (u8, u8),
    device_id: u8,
    [pitch_rate, yaw_rate]: [f32; 2],
    yaw_locked: bool,
) -> Vec<u8> {
    use mavio::dialects::ardupilotmega::enums::GimbalManagerFlags;
    use mavio::dialects::ardupilotmega::messages::GimbalManagerSetPitchyaw;
    let message = GimbalManagerSetPitchyaw {
        target_system,
        target_component,
        flags: GimbalManagerFlags::from_bits_retain(if yaw_locked { YAW_LOCK } else { 0 }),
        gimbal_device_id: device_id,
        pitch: f32::NAN,
        yaw: f32::NAN,
        pitch_rate,
        yaw_rate,
    };
    crate::sim::frame(gcs_system, gcs_component, sequence, &message)
}

/// Turns a gimbal's command into the one PX4's own manager takes (S413,
/// read in PX4 1.17 and run in its simulator). Its
/// `GIMBAL_MANAGER_INFORMATION` says neither difference, so UXDuck goes by
/// the autopilot being a PX4. `ours` is UXDuck's own system and component.
///
/// Taking control names UXDuck itself. PX4 reads -2 as the autopilot, as
/// MAVLink words it for a mission, where ArduPilot reads the sender: PX4
/// accepts the command, puts itself in control and then refuses every
/// movement UXDuck sends.
///
/// A movement holds roll and pitch from the horizon (`ROLL_LOCK`,
/// `PITCH_LOCK`), as UXDuck means a pitch and as MAVSDK sends it. PX4
/// hands the flags on to the gimbal device, and with neither flag it tells
/// the device that roll and pitch are from the vehicle. ArduPilot reads
/// only `YAW_LOCK`.
///
/// Giving control back (-3) and looking at a place go as they are: PX4
/// accepts both.
pub fn for_px4(action: &GimbalAction, wire: &mut Wire, ours: (u8, u8)) {
    let Wire::Long(params) = wire else {
        return;
    };
    match action {
        GimbalAction::TakeControl => {
            params[0] = f32::from(ours.0);
            params[1] = f32::from(ours.1);
        }
        GimbalAction::Point { .. }
        | GimbalAction::Centre
        | GimbalAction::Follow
        | GimbalAction::HoldNorth => params[4] += f32::from(ROLL_LOCK | PITCH_LOCK),
        GimbalAction::ReleaseControl | GimbalAction::LookAt { .. } => {}
    }
}

/// A `GIMBAL_MANAGER_SET_ATTITUDE` that turns a gimbal PX4 manages at
/// rates (S413), in place of [`rates_frame`]: PX4 1.17 has no
/// `GIMBAL_MANAGER_SET_PITCHYAW` and drops one unread. Pitch then yaw in
/// radians a second, up and right positive, with no quaternion and no
/// roll, as MAVSDK streams rates. Roll and pitch stay held from the
/// horizon, and the yaw from north or from the nose as the gimbal last
/// said it is. Zero rates stop it. PX4 takes the message only from
/// whoever is in primary control, drops any other without a word, and
/// stops a gimbal by itself two seconds after the last rates it heard.
#[must_use]
pub fn px4_rates_frame(
    (gcs_system, gcs_component, sequence): (u8, u8, u8),
    (target_system, target_component): (u8, u8),
    device_id: u8,
    [pitch_rate, yaw_rate]: [f32; 2],
    yaw_locked: bool,
) -> Vec<u8> {
    use mavio::dialects::ardupilotmega::enums::GimbalManagerFlags;
    use mavio::dialects::ardupilotmega::messages::GimbalManagerSetAttitude;
    let yaw = if yaw_locked { YAW_LOCK } else { 0 };
    let message = GimbalManagerSetAttitude {
        target_system,
        target_component,
        flags: GimbalManagerFlags::from_bits_retain(ROLL_LOCK | PITCH_LOCK | yaw),
        gimbal_device_id: device_id,
        q: [f32::NAN; 4],
        angular_velocity_x: 0.0,
        angular_velocity_y: pitch_rate,
        angular_velocity_z: yaw_rate,
    };
    crate::sim::frame(gcs_system, gcs_component, sequence, &message)
}

/// A yaw in degrees as `MAV_CMD_DO_GIMBAL_MANAGER_PITCHYAW` takes it, from
/// -180 to 180: a yaw typed from north may be up to 360, and 270 from
/// north is 90 to the left of it. One already within the range is left as
/// it is.
fn within_half_a_turn(yaw_deg: f32) -> f32 {
    if (-180.0..=180.0).contains(&yaw_deg) {
        yaw_deg
    } else {
        (yaw_deg + 180.0).rem_euclid(360.0) - 180.0
    }
}

/// Who holds a gimbal, from who its manager says is in primary control.
#[must_use]
pub fn holder(status: Option<Status>, ours: (u8, u8), autopilot: (u8, u8)) -> GimbalHolder {
    match status.map(|status| status.primary) {
        None => GimbalHolder::Unknown,
        Some((0, 0)) => GimbalHolder::NoOne,
        Some(primary) if primary == ours => GimbalHolder::Uxduck,
        Some(primary) if primary == autopilot => GimbalHolder::Autopilot,
        Some(_) => GimbalHolder::Other,
    }
}

#[cfg(test)]
mod px4_tests;
#[cfg(test)]
mod session_tests;

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: u64 = 1_000_000;

    fn autopilot() -> Key {
        ("udp".into(), "peer".into(), 1, 1)
    }

    fn device() -> Key {
        ("udp".into(), "peer".into(), 1, 154)
    }

    /// What ArduPilot answers once it has found its device (S406).
    fn information(gimbal_device_id: u8) -> Information {
        Information {
            cap_flags: 206_847,
            gimbal_device_id,
            limits: [(-0.489, 0.489), (-1.99, 0.593), (-3.089, 3.089)],
        }
    }

    fn status(primary: (u8, u8)) -> Status {
        Status {
            gimbal_device_id: 1,
            primary,
            secondary: (0, 0),
        }
    }

    fn attitude(flags: u16, gimbal_device_id: u8) -> Attitude {
        Attitude {
            flags,
            q: [1.0, 0.0, 0.0, 0.0],
            gimbal_device_id,
        }
    }

    fn asked(gimbals: &mut Gimbals, now_us: u64, watched: bool) -> Vec<(u8, Request)> {
        gimbals
            .requests(now_us, |_| true, |_| watched)
            .into_iter()
            .map(|(key, request)| (key.3, request))
            .collect()
    }

    #[test]
    fn every_component_is_asked_three_times_until_it_answers() {
        let mut gimbals = Gimbals::default();
        gimbals.heartbeat(&autopilot(), Kind::Autopilot, 0);
        gimbals.heartbeat(&device(), Kind::Gimbal, 0);
        assert_eq!(
            asked(&mut gimbals, 0, false),
            [(1, Request::Information), (154, Request::Information)]
        );
        assert!(asked(&mut gimbals, SECOND, false).is_empty(), "not yet");
        gimbals.information(&autopilot(), information(1), 2 * SECOND);
        // The autopilot answered. The device, which manages nothing, is
        // asked twice more and then left.
        assert_eq!(
            asked(&mut gimbals, 3 * SECOND, false),
            [(154, Request::Information)]
        );
        assert_eq!(
            asked(&mut gimbals, 6 * SECOND, false),
            [(154, Request::Information)]
        );
        let later: Vec<_> = asked(&mut gimbals, 9 * SECOND, false)
            .into_iter()
            .filter(|(component, _)| *component == 154)
            .collect();
        assert!(later.is_empty(), "{later:?}");
        // Nothing is asked of a vehicle that is not known or not heard.
        let mut unknown = Gimbals::default();
        unknown.heartbeat(&autopilot(), Kind::Autopilot, 0);
        assert!(unknown.requests(0, |_| false, |_| false).is_empty());
    }

    #[test]
    fn an_autopilots_gimbal_is_listed_with_its_device_and_who_holds_it() {
        let mut gimbals = Gimbals::default();
        gimbals.heartbeat(&autopilot(), Kind::Autopilot, 0);
        assert!(!gimbals.take_changed());
        gimbals.information(&autopilot(), information(1), SECOND);
        assert!(gimbals.take_changed());
        let [(key, listed)] = gimbals.listed().try_into().expect("one gimbal");
        assert_eq!((key.3, listed.device_id), (1, 1));
        assert_eq!(listed.device_component_id, None, "no device heard");
        assert_eq!(listed.status, None);
        assert_eq!(listed.yaw_locked, None);
        let pitch = listed.limits.pitch_min_deg.expect("a limit");
        assert!((pitch + 114.02).abs() < 0.01, "{pitch}");
        // Its device's heartbeat names the component a camera sits on.
        gimbals.heartbeat(&device(), Kind::Gimbal, SECOND);
        assert!(gimbals.take_changed());
        assert_eq!(gimbals.listed()[0].1.device_component_id, Some(154));
        // Who is in control is told when it changes and not again.
        gimbals.status(&autopilot(), status((255, 190)), 2 * SECOND);
        assert!(gimbals.take_changed());
        gimbals.status(&autopilot(), status((255, 190)), 3 * SECOND);
        assert!(!gimbals.take_changed());
        assert_eq!(gimbals.listed()[0].1.status, Some(status((255, 190))));
        assert_eq!(
            holder(Some(status((255, 190))), (255, 190), (1, 1)),
            GimbalHolder::Uxduck
        );
        assert_eq!(
            holder(Some(status((0, 0))), (255, 190), (1, 1)),
            GimbalHolder::NoOne
        );
        assert_eq!(
            holder(Some(status((1, 1))), (255, 190), (1, 1)),
            GimbalHolder::Autopilot
        );
        assert_eq!(
            holder(Some(status((254, 190))), (255, 190), (1, 1)),
            GimbalHolder::Other
        );
        assert_eq!(holder(None, (255, 190), (1, 1)), GimbalHolder::Unknown);
    }

    #[test]
    fn the_flags_come_from_the_attitude_the_manager_sends_itself() {
        let mut gimbals = Gimbals::default();
        gimbals.heartbeat(&autopilot(), Kind::Autopilot, 0);
        gimbals.heartbeat(&device(), Kind::Gimbal, 0);
        gimbals.information(&autopilot(), information(1), 0);
        gimbals.take_changed();
        // ArduPilot's own: 44 following, 60 with its yaw held.
        gimbals.attitude(&autopilot(), attitude(44, 1));
        assert!(gimbals.take_changed());
        assert_eq!(gimbals.listed()[0].1.yaw_locked, Some(false));
        gimbals.attitude(&autopilot(), attitude(44, 1));
        assert!(!gimbals.take_changed(), "the same flags are not news");
        gimbals.attitude(&autopilot(), attitude(60, 1));
        assert!(gimbals.take_changed());
        assert_eq!(gimbals.listed()[0].1.yaw_locked, Some(true));
        // The device's own account, which calls itself 0, is not the
        // autopilot's gimbal 1: it is left.
        gimbals.attitude(&device(), attitude(12, 0));
        assert!(!gimbals.take_changed());
        assert_eq!(gimbals.listed()[0].1.yaw_locked, Some(true));
        gimbals.attitude(&autopilot(), attitude(46, 1));
        let listed = gimbals.listed()[0].1;
        assert!(listed.neutral && !listed.retracted);
    }

    #[test]
    fn a_gimbal_that_manages_itself_is_its_own_device() {
        let mut gimbals = Gimbals::default();
        gimbals.heartbeat(&device(), Kind::Gimbal, 0);
        gimbals.information(&device(), information(1), 0);
        gimbals.status(&device(), status((0, 0)), 0);
        gimbals.attitude(&device(), attitude(32 | 16, 1));
        let [(key, listed)] = gimbals.listed().try_into().expect("one gimbal");
        assert_eq!((key.3, listed.device_id), (154, 1));
        assert_eq!(listed.device_component_id, Some(154));
        assert_eq!(listed.yaw_locked, Some(true));
        // A manager elsewhere that names a device by its component reads
        // that device's own flags.
        let computer: Key = ("udp".into(), "peer".into(), 1, 191);
        let mut gimbals = Gimbals::default();
        gimbals.heartbeat(&computer, Kind::Other, 0);
        gimbals.heartbeat(&device(), Kind::Gimbal, 0);
        gimbals.information(&computer, information(154), 0);
        gimbals.attitude(&device(), attitude(16, 0));
        let [(key, listed)] = gimbals.listed().try_into().expect("one gimbal");
        assert_eq!((key.3, listed.device_id), (191, 154));
        assert_eq!(listed.device_component_id, Some(154));
        assert_eq!(listed.yaw_locked, Some(true));
    }

    #[test]
    fn the_limits_are_asked_again_since_ardupilot_changes_them() {
        let mut gimbals = Gimbals::default();
        gimbals.heartbeat(&autopilot(), Kind::Autopilot, 0);
        asked(&mut gimbals, 0, false);
        // Before it has found its device: its own defaults.
        let defaults = Information {
            limits: [
                (-0.524, 0.524),
                (-std::f32::consts::FRAC_PI_2, 0.349),
                (-std::f32::consts::PI, std::f32::consts::PI),
            ],
            ..information(1)
        };
        gimbals.information(&autopilot(), defaults, SECOND);
        gimbals.take_changed();
        // How often it is asked what it is, while it says who holds it.
        let again = |gimbals: &mut Gimbals, now_us: u64, watched: bool| {
            gimbals.status(&autopilot(), status((0, 0)), now_us);
            asked(gimbals, now_us, watched)
                .iter()
                .filter(|(_, request)| *request == Request::Information)
                .count()
        };
        assert_eq!(again(&mut gimbals, 10 * SECOND, false), 0);
        assert_eq!(
            again(&mut gimbals, 16 * SECOND, false),
            1,
            "once more after it has settled"
        );
        gimbals.take_changed();
        gimbals.information(&autopilot(), information(1), 16 * SECOND);
        assert!(gimbals.take_changed(), "the device's limits are news");
        gimbals.information(&autopilot(), information(1), 17 * SECOND);
        assert!(!gimbals.take_changed());
        // Not again while no one watches.
        assert_eq!(again(&mut gimbals, 100 * SECOND, false), 0);
        // When watching starts on an old answer, and every half minute
        // while it goes on.
        assert_eq!(again(&mut gimbals, 101 * SECOND, true), 1);
        gimbals.information(&autopilot(), information(1), 101 * SECOND);
        assert_eq!(again(&mut gimbals, 120 * SECOND, true), 0);
        assert_eq!(again(&mut gimbals, 131 * SECOND, true), 1);
        assert_eq!(again(&mut gimbals, 140 * SECOND, true), 0);
        assert_eq!(again(&mut gimbals, 161 * SECOND, true), 1);
    }

    #[test]
    fn a_manager_that_does_not_say_who_is_in_control_is_asked_to() {
        let mut gimbals = Gimbals::default();
        gimbals.heartbeat(&autopilot(), Kind::Autopilot, 0);
        asked(&mut gimbals, 0, false);
        gimbals.information(&autopilot(), information(1), 0);
        assert!(asked(&mut gimbals, 2 * SECOND, false).is_empty(), "not yet");
        let status_asked = (
            1,
            Request::Interval(GIMBAL_MANAGER_STATUS, STATUS_INTERVAL_US),
        );
        assert_eq!(asked(&mut gimbals, 5 * SECOND, false), [status_asked]);
        assert!(asked(&mut gimbals, 6 * SECOND, false).is_empty());
        assert_eq!(asked(&mut gimbals, 10 * SECOND, false), [status_asked]);
        // It says so: nothing more is asked while it goes on saying it.
        gimbals.status(&autopilot(), status((0, 0)), 11 * SECOND);
        assert!(asked(&mut gimbals, 14 * SECOND, false).is_empty());
        // Silent again, as after the autopilot restarted its streams.
        gimbals.status(&autopilot(), status((0, 0)), 14 * SECOND);
        let later = asked(&mut gimbals, 19 * SECOND, false);
        assert!(later.contains(&status_asked), "{later:?}");
    }

    /// Time going by a tenth of a second at a go, with the autopilot
    /// saying where its gimbal points `hz` times a second: the rates
    /// asked of it on the way.
    fn run(gimbals: &mut Gimbals, hz: u64, seconds: (u64, u64), watched: bool) -> Vec<u32> {
        let mut out = Vec::new();
        let mut at = seconds.0 * SECOND;
        let mut next = at;
        while at < seconds.1 * SECOND {
            if at >= next {
                gimbals.attitude(&autopilot(), attitude(44, 1));
                next += SECOND / hz;
            }
            gimbals.status(&autopilot(), status((0, 0)), at);
            for (_, request) in asked(gimbals, at, watched) {
                if let Request::Interval(GIMBAL_DEVICE_ATTITUDE_STATUS, interval_us) = request {
                    out.push(interval_us);
                }
            }
            at += SECOND / 10;
        }
        out
    }

    #[test]
    fn where_it_points_is_asked_ten_times_a_second_only_while_watched() {
        let (fast, slow) = (WATCHED_INTERVAL_US, IDLE_INTERVAL_US);
        let mut gimbals = Gimbals::default();
        gimbals.heartbeat(&autopilot(), Kind::Autopilot, 0);
        asked(&mut gimbals, 0, false);
        gimbals.information(&autopilot(), information(1), 0);
        // Once a second, as every autopilot is asked at first: left so.
        assert!(run(&mut gimbals, 1, (0, 6), false).is_empty());
        // Someone watches: asked at once, and again five seconds later
        // while it is still slow.
        assert_eq!(run(&mut gimbals, 1, (6, 7), true), [fast]);
        assert_eq!(run(&mut gimbals, 1, (7, 12), true), [fast]);
        // It comes ten times a second: nothing more.
        assert!(run(&mut gimbals, 10, (12, 20), true).is_empty());
        // No one watches any more: back to once a second, at once.
        assert_eq!(run(&mut gimbals, 10, (20, 21), false), [slow]);
        assert!(run(&mut gimbals, 1, (21, 30), false).is_empty());
        // A gimbal that never speeds up is asked three times and left.
        assert_eq!(run(&mut gimbals, 1, (30, 60), true), [fast; 3]);
    }

    #[test]
    fn a_manager_gone_is_kept_and_asked_again_when_it_returns() {
        let mut gimbals = Gimbals::default();
        gimbals.heartbeat(&autopilot(), Kind::Autopilot, 0);
        asked(&mut gimbals, 0, false);
        gimbals.information(&autopilot(), information(1), 0);
        gimbals.status(&autopilot(), status((255, 190)), 0);
        gimbals.take_changed();
        gimbals.silent(20 * SECOND, 15 * SECOND, |_| true);
        assert!(!gimbals.take_changed(), "a paused link's is not gone");
        gimbals.silent(20 * SECOND, 15 * SECOND, |_| false);
        assert!(gimbals.take_changed());
        let listed = gimbals.listed()[0].1;
        assert!(!listed.heard);
        assert!(!gimbals.any_heard());
        assert!(asked(&mut gimbals, 21 * SECOND, false).is_empty());
        gimbals.heartbeat(&autopilot(), Kind::Autopilot, 30 * SECOND);
        assert!(gimbals.take_changed());
        let listed = gimbals.listed()[0].1;
        assert!(listed.heard);
        assert_eq!(listed.status, None, "who holds it is read again");
        assert_eq!(
            asked(&mut gimbals, 30 * SECOND, false),
            [(1, Request::Information)]
        );
        gimbals.forget(|key| key.0 == "udp");
        assert!(gimbals.take_changed());
        assert!(gimbals.listed().is_empty());
    }

    #[test]
    fn each_action_is_the_command_ardupilot_accepted() {
        let long = |action| match encode(&action, 1).expect("encodes") {
            (command, Wire::Long(params)) => (command, params),
            other @ (_, Wire::Int { .. }) => panic!("{other:?}"),
        };
        assert_eq!(
            long(GimbalAction::TakeControl),
            (1001, [-2.0, -2.0, -1.0, -1.0, 0.0, 0.0, 1.0])
        );
        assert_eq!(
            long(GimbalAction::ReleaseControl),
            (1001, [-3.0, -3.0, -1.0, -1.0, 0.0, 0.0, 1.0])
        );
        let (command, params) = long(GimbalAction::Point {
            pitch_deg: -45.0,
            yaw_deg: 30.0,
            lock: true,
        });
        assert_eq!(command, 1000);
        assert_eq!(params[..2], [-45.0, 30.0]);
        assert!(params[2].is_nan() && params[3].is_nan(), "no rates");
        assert_eq!(params[4..], [16.0, 0.0, 1.0]);
        // A yaw typed from north may be up to 360°, and the command takes
        // -180° to 180°: 270° from north goes as 90° to the left of it.
        for (typed, sent) in [
            (270.0, -90.0),
            (-270.0, 90.0),
            (360.0, 0.0),
            (190.0, -170.0),
            (180.0, 180.0),
            (-180.0, -180.0),
        ] {
            let (_, params) = long(GimbalAction::Point {
                pitch_deg: -10.0,
                yaw_deg: typed,
                lock: true,
            });
            assert_eq!(params[..2], [-10.0, sent], "{typed}");
            assert_eq!(params[4..5], [16.0]);
        }
        let (_, centre) = long(GimbalAction::Centre);
        assert_eq!([centre[0], centre[1], centre[4]], [0.0; 3]);
        let place = GimbalAction::LookAt {
            latitude_deg: -35.362_938,
            longitude_deg: 149.165_085,
            altitude_amsl_m: Some(584.0),
        };
        assert_eq!(
            encode(&place, 1).expect("encodes"),
            (
                195,
                Wire::Int {
                    frame: 0,
                    params: [1.0, 0.0, 0.0, 0.0],
                    x: -353_629_380,
                    y: 1_491_650_850,
                    z: 584.0,
                }
            )
        );
        let nowhere = GimbalAction::LookAt {
            latitude_deg: 0.0,
            longitude_deg: 0.0,
            altitude_amsl_m: None,
        };
        assert!(encode(&nowhere, 1).is_err(), "a place has a height");
        let wild = GimbalAction::Point {
            pitch_deg: f32::NAN,
            yaw_deg: 0.0,
            lock: false,
        };
        assert!(encode(&wild, 1).is_err());
    }

    #[test]
    fn following_and_holding_north_keep_where_it_points() {
        let filled = |action: GimbalAction, pointing, heading| {
            let (_, mut wire) = encode(&action, 1).expect("encodes");
            keep_pointing(&action, &mut wire, pointing, heading).map(|()| match wire {
                Wire::Long(params) => [params[0], params[1], params[4]],
                Wire::Int { .. } => panic!("not a place"),
            })
        };
        // It points 30° right of a nose that heads 350°: 20° from north.
        let nose = Some((-10.0, 30.0, false));
        assert_eq!(
            filled(GimbalAction::HoldNorth, nose, Some(350.0)),
            Ok([-10.0, 20.0, 16.0])
        );
        assert_eq!(
            filled(GimbalAction::Follow, nose, None),
            Ok([-10.0, 30.0, 0.0])
        );
        // Said from north, and let follow the nose again.
        let north = Some((-10.0, 20.0, true));
        assert_eq!(
            filled(GimbalAction::Follow, north, Some(350.0)),
            Ok([-10.0, 30.0, 0.0])
        );
        assert_eq!(
            filled(GimbalAction::HoldNorth, north, None),
            Ok([-10.0, 20.0, 16.0])
        );
        assert!(filled(GimbalAction::HoldNorth, nose, None).is_err());
        assert!(filled(GimbalAction::Follow, None, Some(0.0)).is_err());
        // Any other action is left as it was encoded.
        let (_, mut wire) = encode(
            &GimbalAction::LookAt {
                latitude_deg: 1.0,
                longitude_deg: 2.0,
                altitude_amsl_m: Some(3.0),
            },
            1,
        )
        .expect("encodes");
        let before = wire;
        assert_eq!(
            keep_pointing(&GimbalAction::Centre, &mut wire, None, None),
            Ok(())
        );
        assert_eq!(wire, before);
    }

    #[test]
    fn where_it_points_reads_the_quaternion_and_its_frame() {
        // 30° down and 45° right.
        let (pitch, yaw) = (-30.0_f32.to_radians() / 2.0, 45.0_f32.to_radians() / 2.0);
        let q = [
            pitch.cos() * yaw.cos(),
            -pitch.sin() * yaw.sin(),
            pitch.sin() * yaw.cos(),
            pitch.cos() * yaw.sin(),
        ];
        let read = |flags| {
            Attitude {
                flags,
                q,
                gimbal_device_id: 1,
            }
            .angles()
            .expect("angles")
        };
        let (pitch, yaw, north) = read(32);
        assert!((pitch + 30.0).abs() < 0.01 && (yaw - 45.0).abs() < 0.01);
        assert!(!north);
        assert!(read(64).2 && read(16).2 && !read(32 | 16).2 && !read(0).2);
        let none = Attitude {
            flags: 0,
            q: [f32::NAN; 4],
            gimbal_device_id: 1,
        };
        assert_eq!(none.angles(), None);
    }

    #[test]
    fn the_device_id_is_read_from_the_end_of_an_attitude_frame() {
        use mavio::dialects::ardupilotmega::enums::{GimbalDeviceErrorFlags, GimbalDeviceFlags};
        use mavio::dialects::ardupilotmega::messages::GimbalDeviceAttitudeStatus;
        let frame = |gimbal_device_id| {
            crate::sim::frame(
                1,
                1,
                0,
                &GimbalDeviceAttitudeStatus {
                    target_system: 0,
                    target_component: 0,
                    time_boot_ms: 1,
                    flags: GimbalDeviceFlags::from_bits_retain(44),
                    q: [1.0, 0.0, 0.0, 0.0],
                    angular_velocity_x: 0.0,
                    angular_velocity_y: 0.0,
                    angular_velocity_z: 0.0,
                    failure_flags: GimbalDeviceErrorFlags::empty(),
                    delta_yaw: 0.5,
                    delta_yaw_velocity: 0.0,
                    gimbal_device_id,
                },
            )
        };
        assert_eq!(attitude_device_id(&frame(1)), 1);
        assert_eq!(attitude_device_id(&frame(154)), 154);
        // MAVLink 2 leaves trailing zeros out: a short frame says 0.
        assert_eq!(attitude_device_id(&frame(0)), 0);
    }

    #[test]
    fn a_gimbal_says_what_is_wrong_with_it_by_its_manager_and_by_its_device() {
        let mut gimbals = Gimbals::default();
        gimbals.heartbeat(&autopilot(), Kind::Autopilot, 0);
        gimbals.heartbeat(&device(), Kind::Gimbal, 0);
        gimbals.information(&autopilot(), information(1), SECOND);
        gimbals.take_changed();
        assert_eq!(gimbals.listed()[0].1.failure_flags, 0);
        // The device's own word, from its own component: the autopilot's
        // first mount is the gimbal at component 154.
        gimbals.failures(&device(), 0, 32);
        assert!(gimbals.take_changed());
        assert_eq!(gimbals.listed()[0].1.failure_flags, 32);
        // The autopilot's account of it is kept apart. One that says
        // nothing is wrong does not hide what the device says.
        gimbals.failures(&autopilot(), 1, 0);
        assert!(!gimbals.take_changed());
        assert_eq!(gimbals.listed()[0].1.failure_flags, 32);
        gimbals.failures(&autopilot(), 1, 16);
        assert!(gimbals.take_changed());
        assert_eq!(gimbals.listed()[0].1.failure_flags, 48);
        // Said again changes nothing, and a gimbal that is well again says so.
        gimbals.failures(&device(), 0, 32);
        assert!(!gimbals.take_changed());
        gimbals.failures(&device(), 0, 0);
        gimbals.failures(&autopilot(), 1, 0);
        assert!(gimbals.take_changed());
        assert_eq!(gimbals.listed()[0].1.failure_flags, 0);
        // Another vehicle's device, and a component that is no mount's
        // device, say nothing of this gimbal.
        gimbals.failures(&("udp".into(), "peer".into(), 2, 154), 0, 32);
        gimbals.failures(&("udp".into(), "peer".into(), 1, 100), 0, 32);
        assert!(!gimbals.take_changed());
        assert_eq!(gimbals.listed()[0].1.failure_flags, 0);
    }

    #[test]
    fn a_unit_that_manages_its_own_gimbal_says_what_is_wrong_with_it_itself() {
        // A camera and gimbal in one answers as its own manager, for the
        // gimbal it calls 1 (ADR-0063 Y16).
        let unit: Key = ("udp".into(), "peer".into(), 1, 100);
        let mut gimbals = Gimbals::default();
        gimbals.heartbeat(&autopilot(), Kind::Autopilot, 0);
        gimbals.heartbeat(&unit, Kind::Other, 0);
        gimbals.information(&unit, information(1), SECOND);
        gimbals.take_changed();
        gimbals.failures(&unit, 1, 32);
        assert!(gimbals.take_changed());
        let [(key, listed)] = gimbals.listed().try_into().expect("one gimbal");
        assert_eq!(
            (key.3, listed.device_id, listed.failure_flags),
            (100, 1, 32)
        );
        assert_eq!(listed.device_component_id, Some(100));
    }

    #[test]
    fn what_is_wrong_is_read_from_an_attitude_frame_and_is_nothing_when_left_off() {
        use mavio::dialects::ardupilotmega::enums::{GimbalDeviceErrorFlags, GimbalDeviceFlags};
        use mavio::dialects::ardupilotmega::messages::GimbalDeviceAttitudeStatus;
        let frame = |failure_flags: u16, gimbal_device_id| {
            crate::sim::frame(
                1,
                154,
                0,
                &GimbalDeviceAttitudeStatus {
                    target_system: 0,
                    target_component: 0,
                    time_boot_ms: 1,
                    flags: GimbalDeviceFlags::from_bits_retain(44),
                    q: [1.0, 0.0, 0.0, 0.0],
                    angular_velocity_x: 0.0,
                    angular_velocity_y: 0.0,
                    angular_velocity_z: 0.0,
                    failure_flags: GimbalDeviceErrorFlags::from_bits_retain(failure_flags),
                    delta_yaw: 0.0,
                    delta_yaw_velocity: 0.0,
                    gimbal_device_id,
                },
            )
        };
        assert_eq!(attitude_failure_flags(&frame(32, 1)), 32);
        assert_eq!(attitude_failure_flags(&frame(32 | 512, 0)), 544);
        // MAVLink 2 leaves trailing zeros out: a short frame says nothing
        // is wrong.
        assert_eq!(attitude_failure_flags(&frame(0, 0)), 0);
        assert_eq!(attitude_failure_flags(&[]), 0);
    }
}

//! The live view of one MAVLink network: the peer table, component addresses,
//! vehicle grouping and presence (ADR-0004). Logic over frames and time; the
//! caller supplies both and carries out the effects.

use std::collections::BTreeMap;

use uxduck_core::domain::modes::AvailableMode;
use uxduck_core::domain::parameter::same_value;
use uxduck_core::domain::telemetry::{Altitude, AltitudeFrame, VehicleTelemetry};

use crate::decode::{Message, decode, firmware_commit, firmware_version};
use crate::files::{self, FtpClient};
use crate::framing::split;
use crate::identity::{provisional_vid, simulated_vid, uid_text, vid_from_uid};
use crate::producers::{Produced, Producer, core_producers};
use crate::sim::{Route, request_autopilot_version_frame, set_message_interval_frame};
use crate::transfers::{
    Failure, MissionDownload, MissionUpload, Out, Param, ParamDownload, ParamWrite, Step, Timing,
};

/// Parameter writes that arrive together go up as one file when there are
/// at least this many (S121, ADR-0027 F13); fewer go by `PARAM_SET`.
pub const PARAM_FILE_WRITES: usize = 8;

/// How far a vehicle's boot instant may move before it counts as a reboot:
/// link delay and clock drift stay well inside it.
const REBOOT_TOLERANCE_US: u64 = 5_000_000;

/// How far a vehicle's time since boot may move on across a silence for it
/// to count as paused rather than restarted.
const PAUSE_SLACK_MS: u32 = 5_000;

const MAV_CMD_NAV_TAKEOFF: u16 = 22;
const MAV_CMD_CONDITION_YAW: u16 = 115;
/// `AVAILABLE_MODES`: a vehicle's own list of modes, asked for by
/// `MAV_CMD_REQUEST_MESSAGE`.
const AVAILABLE_MODES: u32 = 435;

/// Telemetry the daemon asks each autopilot to stream, as (message id,
/// interval in µs), when it first hears it on a link it may write to.
/// ArduPilot's Copter, Heli and Blimp stream nothing by default (every
/// `SRn_` rate is 0), so whatever the app needs from them has to be here.
pub const STREAM_RATES: [(u32, u32); 19] = [
    (33, 200_000),    // GLOBAL_POSITION_INT, 5 Hz
    (30, 100_000),    // ATTITUDE, 10 Hz
    (1, 1_000_000),   // SYS_STATUS, 1 Hz
    (24, 500_000),    // GPS_RAW_INT, 2 Hz
    (74, 250_000),    // VFR_HUD, 4 Hz
    (36, 200_000),    // SERVO_OUTPUT_RAW, 5 Hz: surfaces and motors (S35)
    (42, 1_000_000),  // MISSION_CURRENT, 1 Hz: the item flown to (S34)
    (62, 500_000),    // NAV_CONTROLLER_OUTPUT, 2 Hz: distance to the waypoint (S43)
    (193, 1_000_000), // EKF_STATUS_REPORT, 1 Hz (S72)
    (241, 1_000_000), // VIBRATION, 1 Hz (S72)
    (147, 1_000_000), // BATTERY_STATUS, 1 Hz: battery used (S72)
    (136, 1_000_000), // TERRAIN_REPORT, 1 Hz: the height above terrain
    (87, 500_000),    // POSITION_TARGET_GLOBAL_INT, 2 Hz: where it is going
    (242, 5_000_000), // HOME_POSITION, every 5 s: otherwise only when it is set
    (168, 1_000_000), // WIND, 1 Hz (S72)
    (132, 500_000),   // DISTANCE_SENSOR, 2 Hz: the rangefinder (S72)
    // TERRAIN_REQUEST (S118): ArduPilot asks a ground station for terrain
    // only on a channel streaming this, and itself at most every 2 s.
    (133, 1_000_000),
    // FENCE_STATUS, 1 Hz (S132): a breach, while the vehicle has a fence.
    (162, 1_000_000),
    // GIMBAL_DEVICE_ATTITUDE_STATUS, 1 Hz: where its camera points, for the
    // 3D map's Drone camera, while it has a gimbal (2026-10-18; 1 Hz to
    // spare a telemetry radio, maintainer).
    (285, 1_000_000),
];

/// A terrain report older than this no longer gives the fused position an
/// above-ground altitude: the stream stopped, or the data went.
const TERRAIN_FRESH_US: u64 = 5_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// Heartbeat age after which a vehicle is `stale`.
    pub stale_after_us: u64,
    /// Heartbeat age after which a vehicle is `offline`.
    pub offline_after_us: u64,
    /// A peer silent this long is forgotten; its vehicles keep their last address.
    pub peer_expiry_us: u64,
    /// How often to ask again for `AUTOPILOT_VERSION`, and how many times.
    pub version_retry_us: u64,
    pub version_attempts: u8,
    /// How long to wait for a command's acknowledgement, and how many times
    /// to send it (the MAVLink command protocol).
    pub command_timeout_us: u64,
    pub command_attempts: u8,
    /// After a vehicle comes back online, how long to wait before judging
    /// its streams (ADR-0004 rate reconciliation).
    pub stream_settle_us: u64,
    /// The parameter and mission protocols.
    pub transfers: Timing,
    /// The system id UXDuck sends as, and the one replies are addressed
    /// to: a daemon setting, [`crate::GCS_SYSTEM_ID`] unless changed.
    pub gcs_system_id: u8,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            stale_after_us: 3_000_000,
            offline_after_us: 15_000_000,
            peer_expiry_us: 300_000_000,
            version_retry_us: 3_000_000,
            version_attempts: 5,
            command_timeout_us: 1_500_000,
            command_attempts: 3,
            stream_settle_us: 2_000_000,
            transfers: Timing::default(),
            gcs_system_id: crate::GCS_SYSTEM_ID,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    Online,
    Stale,
    Offline,
}

/// What the session knows about one vehicle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    /// The VID: derived from the UID once known, provisional before.
    pub id: String,
    /// Earlier ids of the same vehicle (a provisional id replaced by the derived one).
    pub aliases: Vec<String>,
    pub link_id: String,
    /// The link-level source, such as `192.168.1.20:14550`.
    pub peer: String,
    pub system_id: u8,
    pub component_ids: Vec<u8>,
    pub autopilot: String,
    pub vehicle_type: String,
    pub uid: Option<String>,
    pub firmware: Option<String>,
    /// The first 8 characters of its git hash, when it says.
    pub firmware_commit: Option<String>,
    pub presence: Presence,
    pub last_heartbeat_us: u64,
    /// From the heartbeat's base mode.
    pub armed: bool,
    /// The flight mode by name, when the vehicle type's modes are known.
    pub mode: Option<String>,
}

/// A command to carry out for the engine.
#[derive(Clone, Debug, PartialEq)]
pub struct CommandOrder {
    pub receipt_id: String,
    pub vehicle_id: String,
    pub command: uxduck_core::domain::command::Command,
    pub force: bool,
}

/// How a command ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandOutcome {
    Accepted,
    /// The acknowledgement's result, in words.
    Rejected(String),
    Timeout,
    NotSent(String),
}

/// What the caller must do.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// A vehicle appeared or changed in a way worth recording.
    Vehicle(Box<Observation>),
    /// A vehicle asked for terrain (`TERRAIN_REQUEST`, S118): the groups of
    /// a block of its grid it lacks. It asks again for what is still
    /// missing.
    TerrainRequest {
        vehicle_id: String,
        block: crate::terrain::Block,
        mask: u64,
    },
    /// A vehicle's answer to a `TERRAIN_CHECK` (S119): `Some(None)` when
    /// it has no terrain there, `None` when it did not answer.
    TerrainChecked {
        vehicle_id: String,
        lat: i32,
        lon: i32,
        height: Option<Option<f32>>,
    },
    /// A vehicle listed its own modes (`AVAILABLE_MODES`), whole, and the
    /// list differs from the last one it gave.
    Modes {
        vehicle_id: String,
        modes: Vec<AvailableMode>,
    },
    /// What changed in a vehicle's telemetry since the last tick.
    Telemetry(VehicleTelemetry),
    /// An aircraft the vehicle's receiver heard (`ADSB_VEHICLE`, S168).
    Traffic {
        vehicle_id: String,
        report: uxduck_core::domain::traffic::TrafficReport,
    },
    /// A command ended.
    CommandDone {
        receipt_id: String,
        outcome: CommandOutcome,
        attempts: u8,
    },
    /// When the vehicle's autopilot last booted, in the caller's clock:
    /// once it is known (and the vehicle has its lasting id), and again
    /// after each reboot.
    Booted {
        vehicle_id: String,
        boot_instant_us: u64,
        /// Seen restarting in this session, rather than first heard.
        restarted: bool,
    },
    /// A parameter download ended. By MAVLink FTP each parameter's default
    /// comes too (S74); by the list, none do.
    Parameters {
        vehicle_id: String,
        result: Result<Vec<Param>, Failure>,
        defaults: BTreeMap<String, f32>,
    },
    /// A parameter the vehicle reported outside a download: it changed
    /// it itself, another ground station did, or it answered a read.
    /// `station` when another ground station was heard lately.
    ParamValue {
        vehicle_id: String,
        param: Param,
        station: bool,
    },
    /// A file job's progress or end (S121): `event` is never
    /// `Parameters` or `ParamsWritten`, which become the effects above.
    File {
        vehicle_id: String,
        event: files::Event,
    },
    /// A parameter write ended: the value the vehicle now reports.
    ParamWritten {
        order_id: String,
        result: Result<Param, Failure>,
        attempts: u8,
    },
    /// A mission download ended.
    Mission {
        vehicle_id: String,
        result: Result<Vec<crate::decode::MissionItemInt>, Failure>,
    },
    /// A fence (1) or rally points (2) download ended (S128).
    MissionPart {
        vehicle_id: String,
        kind: u8,
        result: Result<Vec<crate::decode::MissionItemInt>, Failure>,
    },
    /// A mission upload ended.
    MissionUploaded {
        order_id: String,
        result: Result<(), Failure>,
    },
    /// How far a transfer has come (S127): sent as it goes, and when it
    /// asks again after a silence (`attempt` above 1).
    Progress {
        vehicle_id: String,
        transfer: TransferKind,
        done: u32,
        total: Option<u32>,
        attempt: u8,
    },
    /// Send these bytes to this peer on this link.
    Send {
        link_id: String,
        peer: String,
        bytes: Vec<u8>,
    },
    /// A message from the vehicle (`STATUSTEXT`), whole: the parts of a
    /// long one joined (S26).
    StatusText {
        vehicle_id: String,
        component_id: u8,
        /// `MAV_SEVERITY`: 0 emergency to 7 debug.
        severity: u8,
        text: String,
    },
    /// A vehicle's camera named a video stream (S39).
    VideoStream {
        vehicle_id: String,
        component_id: u8,
        stream_id: u8,
        /// `VIDEO_STREAM_TYPE`.
        kind: u8,
        uri: String,
    },
}

/// A long message's parts: link, peer, system, component and message id.
type TextKey = (String, String, u8, u8, u16);

/// The end of a download of one kind: the mission, or a part (S128).
fn part_effect(
    vehicle_id: String,
    kind: u8,
    result: Result<Vec<crate::decode::MissionItemInt>, Failure>,
) -> Effect {
    if kind == 0 {
        Effect::Mission { vehicle_id, result }
    } else {
        Effect::MissionPart {
            vehicle_id,
            kind,
            result,
        }
    }
}

/// Which transfer a progress report or a cancel is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferKind {
    Parameters,
    MissionDownload,
    MissionUpload,
}

fn as_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The receipt fence belongs to the operation, not its most recent retry.
#[derive(Clone, Debug)]
struct PendingTransfer<T> {
    issued_us: u64,
    protocol: T,
}

impl<T> PendingTransfer<T> {
    fn new(protocol: T, issued_us: u64) -> Self {
        Self {
            issued_us,
            protocol,
        }
    }
    fn accepts(&self, received_us: u64) -> bool {
        received_us >= self.issued_us
    }
}

#[derive(Clone, Debug)]
struct Vehicle {
    observation: Observation,
    autopilot_component: u8,
    version_requests: u8,
    last_request_us: u64,
    /// Produced since the last tick; sent and cleared on the next.
    pending: Produced,
    /// When it last came online, for the stream settle window.
    online_since_us: u64,
    /// Each desired stream's state, by message id.
    streams: BTreeMap<u32, Stream>,
    /// When the autopilot booted, from `time_boot_ms`, and whether the
    /// caller has been told.
    boot_instant_us: Option<u64>,
    boot_announced: bool,
    /// The latest `time_boot_ms` heard: going back means a restart.
    last_time_boot_ms: u32,
    params: Option<PendingTransfer<ParamDownload>>,
    rc_input: crate::rc_input::RcInput,
    rc_overridden: [bool; 18],
    /// MAVLink FTP: the parameter file, tried before the list (S74), and
    /// the vehicle's files (S121).
    ftp: FtpClient,
    /// The parameters came by file, from ArduPilot: writes may go up as one.
    params_by_file: bool,
    /// Writes waiting for the next tick, to go together (ADR-0027 F13).
    batch: Vec<(String, Param)>,
    writes: Vec<PendingTransfer<ParamWrite>>,
    mission_download: Option<PendingTransfer<MissionDownload>>,
    mission_upload: Option<PendingTransfer<MissionUpload>>,
    /// Home's altitude above mean sea level, from `GLOBAL_POSITION_INT`,
    /// for PX4's take-off.
    home_amsl_m: Option<f32>,
    /// The latest fused position, retained across telemetry flushes, for a
    /// simulated submarine's explicit Go to heading.
    position: Option<(f64, f64, u64)>,
    heading: Option<(f64, u64)>,
    /// The terrain's height above mean sea level under the vehicle, from
    /// the latest `TERRAIN_REPORT` with data, and when it came.
    terrain: Option<(f64, u64)>,
    /// The modes it lists itself (`AVAILABLE_MODES`), once every one has come.
    modes: Vec<AvailableMode>,
    /// The ones heard so far of a list being sent, by index.
    modes_heard: BTreeMap<u8, AvailableMode>,
    /// `AVAILABLE_MODES_MONITOR`'s count of changes, as last heard.
    modes_seq: Option<u8>,
}

impl Vehicle {
    fn new(
        (link_id, peer, system_id): &(String, String, u8),
        component_id: u8,
        autopilot: &str,
        mav_type: &str,
        now_us: u64,
    ) -> Self {
        Self {
            observation: Observation {
                id: provisional_vid(link_id, peer, *system_id),
                aliases: Vec::new(),
                link_id: link_id.clone(),
                peer: peer.clone(),
                system_id: *system_id,
                component_ids: vec![component_id],
                autopilot: autopilot.to_owned(),
                vehicle_type: mav_type.to_owned(),
                uid: None,
                firmware: None,
                firmware_commit: None,
                presence: Presence::Offline,
                last_heartbeat_us: now_us,
                armed: false,
                mode: None,
            },
            autopilot_component: component_id,
            version_requests: 0,
            last_request_us: 0,
            pending: Produced::default(),
            online_since_us: now_us,
            streams: STREAM_RATES
                .iter()
                .map(|(id, _)| (*id, Stream::default()))
                .collect(),
            boot_instant_us: None,
            boot_announced: false,
            last_time_boot_ms: 0,
            params: None,
            rc_input: crate::rc_input::RcInput::default(),
            rc_overridden: [false; 18],
            ftp: FtpClient::new(),
            params_by_file: false,
            batch: Vec::new(),
            writes: Vec::new(),
            mission_download: None,
            mission_upload: None,
            home_amsl_m: None,
            position: None,
            heading: None,
            terrain: None,
            modes: Vec::new(),
            modes_heard: BTreeMap::new(),
            modes_seq: None,
        }
    }

    /// `ArduSub` ignores reposition's yaw: apply the heading separately.
    fn go_to_heading(
        &self,
        command: &uxduck_core::domain::command::Command,
        now_us: u64,
    ) -> Result<Option<(u16, crate::commands::Wire)>, &'static str> {
        use uxduck_core::domain::command::{Command, GoToHeading};
        let Command::GoTo {
            latitude_deg,
            longitude_deg,
            heading,
            ..
        } = command
        else {
            return Ok(None);
        };
        let current = || {
            self.heading
                .filter(|(_, at)| now_us.saturating_sub(*at) <= 5_000_000)
                .map(|(degrees, _)| degrees)
                .ok_or("a fresh heading is required to keep the current heading")
        };
        let degrees = match heading.unwrap_or(GoToHeading::FaceDestination) {
            GoToHeading::KeepCurrent => current()?,
            GoToHeading::Set { degrees } => degrees.rem_euclid(360.0),
            GoToHeading::FaceDestination => {
                let Some((lat, lon, _)) = self
                    .position
                    .filter(|(_, _, at)| now_us.saturating_sub(*at) <= 5_000_000)
                else {
                    return Err("a fresh fused position is required to face the destination");
                };
                match heading_to(lat, lon, *latitude_deg, *longitude_deg) {
                    Some(bearing) => f64::from(bearing),
                    // A depth-only move keeps the measured heading, including
                    // when an earlier yaw command was still turning the nose.
                    None => current()?,
                }
            }
        };
        #[allow(
            clippy::cast_possible_truncation,
            reason = "a validated heading in degrees"
        )]
        let degrees = degrees as f32;
        Ok(Some((
            MAV_CMD_CONDITION_YAW,
            crate::commands::Wire::Long([degrees, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
        )))
    }

    /// Remembers the terrain's height under the vehicle from a
    /// `TERRAIN_REPORT`, or forgets it when the autopilot has no data for
    /// where it is.
    fn note_terrain(&mut self, message: &Message, now_us: u64) {
        if let &Message::TerrainReport {
            spacing,
            terrain_height,
            ..
        } = message
        {
            self.terrain = (spacing > 0 && terrain_height.is_finite())
                .then(|| (f64::from(terrain_height), now_us));
        }
    }

    /// Gives each fused position produced since the last tick its altitude
    /// above the ground, from its altitude above mean sea level and the
    /// terrain under it, as ArduPilot's own height above terrain is worked
    /// out, while the terrain report is fresh.
    fn add_above_ground(&mut self, now_us: u64) {
        let Some((terrain_m, at_us)) = self.terrain else {
            return;
        };
        if now_us.saturating_sub(at_us) > TERRAIN_FRESH_US {
            return;
        }
        for position in self
            .pending
            .positions
            .iter_mut()
            .filter(|p| p.source == "fused")
        {
            if position
                .altitudes
                .iter()
                .any(|a| a.frame == AltitudeFrame::AboveGround)
            {
                continue;
            }
            if let Some(amsl) = position
                .altitudes
                .iter()
                .find(|a| a.frame == AltitudeFrame::Amsl)
            {
                position.altitudes.push(Altitude {
                    meters: amsl.meters - terrain_m,
                    frame: AltitudeFrame::AboveGround,
                });
            }
        }
    }
}

/// One row of the rate table for one vehicle (ADR-0004): desired state,
/// reconciled only while heartbeats arrive on time.
#[derive(Clone, Copy, Debug, Default)]
struct Stream {
    last_seen_us: Option<u64>,
    /// Requests sent since it was last seen.
    attempts: u8,
    next_try_us: u64,
    /// Given up until it is received or the vehicle next appears.
    unavailable: bool,
}

/// Minimum silence before recovering a stream: brief transport delays must
/// not repeatedly reset a rate that the autopilot already accepted.
const STREAM_SILENCE_US: u64 = 5_000_000;

/// Waits between re-requests of a missing stream after the first (which
/// comes after at least five seconds and three missed intervals); then it is
/// marked unavailable.
const STREAM_BACKOFF_US: [u64; 3] = [5_000_000, 15_000_000, 60_000_000];

/// Rows the autopilot sends only while it has one: a target while it has a
/// place to go, home once it is set, wind once it can estimate it (a copter
/// needs an airspeed sensor or drag fusion), distances while a rangefinder
/// or proximity sensor is fitted and reading. Their silence says nothing
/// about the rate, so they are never asked for again on it. A terrain
/// request comes only while terrain is missing (S118), the fence's status
/// only while there is a fence (S132), a gimbal's attitude only while it has
/// one.
const SENT_WHILE_SET: [u32; 7] = [87, 242, 168, 132, 133, 162, 285];

#[derive(Clone, Debug)]
struct PendingCommand {
    order: CommandOrder,
    key: (String, String, u8),
    component: u8,
    command: u16,
    wire: crate::commands::Wire,
    attempts: u8,
    sent_us: u64,
    issued_us: u64,
    /// Go to on a simulated `ArduSub` needs a separate acknowledged yaw
    /// command: its `DO_REPOSITION` handler ignores the heading parameter.
    follow_up: Option<(u16, crate::commands::Wire)>,
    first_command: u16,
    completed_attempts: u8,
}

/// Initial great-circle bearing. A destination within one metre keeps the
/// existing heading, avoiding an arbitrary rotation for a depth-only Go to.
fn heading_to(lat: f64, lon: f64, target_lat: f64, target_lon: f64) -> Option<f32> {
    let (lat, target_lat) = (lat.to_radians(), target_lat.to_radians());
    let delta_lon = (target_lon - lon).to_radians();
    let a = ((target_lat - lat) / 2.0).sin().powi(2)
        + lat.cos() * target_lat.cos() * (delta_lon / 2.0).sin().powi(2);
    if 2.0 * 6_371_000.0 * a.clamp(0.0, 1.0).sqrt().asin() < 1.0 {
        return None;
    }
    let y = delta_lon.sin() * target_lat.cos();
    let x = lat.cos() * target_lat.sin() - lat.sin() * target_lat.cos() * delta_lon.cos();
    #[allow(clippy::cast_possible_truncation, reason = "a heading in degrees")]
    Some(y.atan2(x).to_degrees().rem_euclid(360.0) as f32)
}

#[derive(Debug, Default)]
pub struct Session {
    config: Config,
    producers: Vec<Box<dyn Producer>>,
    commands: Vec<PendingCommand>,
    /// By `(link, peer, system id)`: one autopilot per address.
    vehicles: BTreeMap<(String, String, u8), Vehicle>,
    /// Last time each peer was heard, by `(link, peer)`.
    peers: BTreeMap<(String, String), u64>,
    sequence: u8,
    /// Camera components, asked for their video streams (S39).
    cameras: crate::camera::Cameras,
    /// Long messages being put together, by vehicle, component and id:
    /// their parts so far, and when the last came.
    texts: BTreeMap<TextKey, (BTreeMap<u8, String>, u64)>,
    /// Other ground stations heard, by `(link, peer, system id)`: when
    /// last. UXDuck's own heartbeat looped back counts too, as it cannot
    /// be told from another's with the same ids.
    stations: BTreeMap<(String, String, u8), u64>,
    /// Simulator links, whose vehicles take their ids from the link.
    simulated: std::collections::BTreeSet<String>,
    /// Simulators' links that are paused (`sim/pause`).
    paused: std::collections::BTreeSet<String>,
    /// Terrain answers waiting to go, paced per vehicle (S118).
    terrain: crate::terrain::Outbox,
    /// `TERRAIN_CHECK`s waiting for their report (S119), by vehicle: the
    /// point and when it was asked.
    terrain_checks: BTreeMap<String, Vec<(i32, i32, u64)>>,
    /// Frames that were not whole frames or failed their checksum.
    pub dropped: u64,
}

/// How long a ground station counts as there after its last heartbeat,
/// which comes once a second.
const STATION_US: u64 = 10_000_000;

impl Session {
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            config,
            producers: core_producers(),
            ..Self::default()
        }
    }

    /// The system id to send as from now on, and to take replies for.
    pub fn set_gcs_system_id(&mut self, id: u8) {
        self.config.gcs_system_id = id;
    }

    /// Marks a link as a simulator's: its vehicles are new vehicles, not
    /// whatever another simulation of the same build was.
    pub fn simulated(&mut self, link_id: &str) {
        self.simulated.insert(link_id.to_owned());
    }

    /// Pauses a simulator's link or lets it run again (`sim/pause`). A
    /// paused simulator sends nothing, which is not a lost vehicle: its
    /// vehicles keep their presence and nothing is asked of them again.
    /// Nothing is sent to it either ([`Session::is_paused`]), since it would
    /// read everything at once as it resumes. Resuming counts as a
    /// heartbeat and gives its streams time to come back before they are
    /// asked for.
    pub fn pause(&mut self, link_id: &str, paused: bool, now_us: u64) {
        if paused {
            self.paused.insert(link_id.to_owned());
            return;
        }
        if !self.paused.remove(link_id) {
            return;
        }
        for vehicle in self
            .vehicles
            .values_mut()
            .filter(|v| v.observation.link_id == link_id)
        {
            if vehicle.observation.presence != Presence::Offline {
                vehicle.observation.last_heartbeat_us = now_us;
                vehicle.online_since_us = now_us;
            }
        }
    }

    /// Whether a link is a paused simulator's: nothing is sent on it.
    #[must_use]
    pub fn is_paused(&self, link_id: &str) -> bool {
        self.paused.contains(link_id)
    }

    /// A frame from a forward, which the session does not read otherwise:
    /// a ground station's heartbeat there means one may change parameters.
    pub fn forwarded(&mut self, link_id: &str, peer: &str, frame: &[u8], now_us: u64) {
        if crate::framing::header(frame).is_none_or(|h| h.message_id != 0) {
            return;
        }
        if let Some(decoded) = decode(frame)
            && let Message::Heartbeat { mav_type, .. } = decoded.message
            && mav_type == "Gcs"
        {
            let key = (link_id.to_owned(), peer.to_owned(), decoded.system_id);
            self.stations.insert(key, now_us);
        }
    }

    /// Whether another ground station was heard lately.
    fn station_heard(&self, now_us: u64) -> bool {
        self.stations
            .values()
            .any(|at| now_us.saturating_sub(*at) <= STATION_US)
    }

    /// Adds a producer after the core ones, as a plugin's estimator would be.
    pub fn add_producer(&mut self, producer: Box<dyn Producer>) {
        self.producers.push(producer);
    }

    /// Peers heard from within the expiry, by link.
    #[must_use]
    pub fn peers(&self, link_id: &str) -> Vec<String> {
        self.peers
            .keys()
            .filter(|(link, _)| link == link_id)
            .map(|(_, peer)| peer.clone())
            .collect()
    }

    fn next_sequence(&mut self) -> u8 {
        self.sequence = self.sequence.wrapping_add(1);
        self.sequence
    }

    /// Handles one datagram (or chunk) from `peer` on `link_id`.
    pub fn ingest(&mut self, link_id: &str, peer: &str, bytes: &[u8], now_us: u64) -> Vec<Effect> {
        self.ingest_received(link_id, peer, bytes, now_us, now_us)
    }

    /// Handles queued bytes without treating their receipt time as the present.
    /// Telemetry keeps its original timestamp; presence uses processing time.
    #[allow(clippy::too_many_lines, reason = "one arm per kind of message")]
    pub fn ingest_received(
        &mut self,
        link_id: &str,
        peer: &str,
        bytes: &[u8],
        received_us: u64,
        processed_us: u64,
    ) -> Vec<Effect> {
        self.peers
            .insert((link_id.to_owned(), peer.to_owned()), received_us);
        let split = split(bytes);
        self.dropped += u64::from(split.junk > 0);
        let mut effects = Vec::new();
        for frame in split.frames {
            let Some(decoded) = decode(frame) else {
                self.dropped += 1;
                continue;
            };
            let key = (link_id.to_owned(), peer.to_owned(), decoded.system_id);
            if let Some((system, component)) = decoded.mission_target
                && ((system != 0 && system != self.config.gcs_system_id)
                    || (component != 0 && component != crate::GCS_COMPONENT_ID)
                    || self
                        .vehicles
                        .get(&key)
                        .is_none_or(|vehicle| vehicle.autopilot_component != decoded.component_id))
            {
                // A shared link may carry another ground station's transfer,
                // or mission traffic from a different on-board component.
                continue;
            }
            if let (Some(vehicle), Some(header)) =
                (self.vehicles.get_mut(&key), crate::framing::header(frame))
                && let Some(stream) = vehicle.streams.get_mut(&header.message_id)
            {
                // Receiving it proves the stream is available again. Clear
                // the old retry deadline and exhausted state as well, so a
                // later interruption starts a fresh recovery cycle.
                *stream = Stream {
                    last_seen_us: Some(received_us),
                    ..Stream::default()
                };
            }
            match decoded.message {
                Message::CommandAck {
                    command,
                    result,
                    target_system,
                    ..
                } => {
                    if target_system == self.config.gcs_system_id || target_system == 0 {
                        effects.extend(self.acknowledged(&key, command, result, received_us));
                    }
                }
                Message::Heartbeat {
                    mav_type,
                    autopilot,
                    base_mode,
                    custom_mode,
                    ..
                } => {
                    let beat = Beat {
                        mav_type,
                        autopilot,
                        armed: base_mode & 0x80 != 0,
                        custom_mode,
                    };
                    effects.extend(self.any_heartbeat(
                        &key,
                        decoded.component_id,
                        &beat,
                        received_us,
                        processed_us,
                    ));
                }
                Message::AutopilotVersion {
                    uid,
                    uid2,
                    flight_sw_version,
                    flight_custom_version,
                } => effects.extend(self.version(
                    &key,
                    uid,
                    uid2,
                    (flight_sw_version, flight_custom_version),
                )),
                message @ (Message::ParamValue { .. }
                | Message::Ftp { .. }
                | Message::MissionCount { .. }
                | Message::MissionItem(_)
                | Message::MissionRequest { .. }
                | Message::MissionAck { .. }) => {
                    effects.extend(self.transfer(&key, message, received_us, processed_us));
                }
                Message::VideoStream {
                    stream_id,
                    kind,
                    uri,
                    ..
                } => {
                    effects.extend(self.video_stream(
                        &key,
                        decoded.component_id,
                        stream_id,
                        kind,
                        uri,
                    ));
                }
                message @ Message::StatusText { .. } => {
                    effects.extend(self.status_text(
                        &key,
                        decoded.component_id,
                        message,
                        received_us,
                    ));
                }
                Message::AvailableModes {
                    number_modes,
                    mode_index,
                    custom_mode,
                    properties,
                    name,
                } => {
                    let mode = AvailableMode::new(custom_mode, &name, properties);
                    effects.extend(self.available_mode(&key, number_modes, mode_index, mode));
                }
                Message::AvailableModesMonitor { seq } => {
                    effects.extend(self.modes_changed(&key, seq));
                }
                // Answered by the engine from UXDuck's own terrain (S118).
                Message::TerrainRequest {
                    lat,
                    lon,
                    grid_spacing,
                    mask,
                } => {
                    if let Some(vehicle) = self.vehicles.get(&key) {
                        effects.push(Effect::TerrainRequest {
                            vehicle_id: vehicle.observation.id.clone(),
                            block: crate::terrain::Block {
                                lat,
                                lon,
                                spacing: grid_spacing,
                            },
                            mask,
                        });
                    }
                }
                Message::Other { .. } => {}
                ref message @ Message::AdsbVehicle { .. } => {
                    if let (Some(vehicle), Some(report)) = (
                        self.vehicles.get(&key),
                        crate::traffic::report(message, received_us),
                    ) {
                        effects.push(Effect::Traffic {
                            vehicle_id: vehicle.observation.id.clone(),
                            report,
                        });
                    }
                }
                Message::TerrainReport {
                    lat,
                    lon,
                    spacing,
                    terrain_height,
                    ..
                } if self.checked(&key, lat, lon) => {
                    // The answer to a check is not the ground under it.
                    let vehicle_id = self.vehicles.get(&key).map(|v| v.observation.id.clone());
                    if let Some(vehicle_id) = vehicle_id {
                        effects.push(Effect::TerrainChecked {
                            vehicle_id,
                            lat,
                            lon,
                            height: Some(
                                (spacing > 0 && terrain_height.is_finite())
                                    .then_some(terrain_height),
                            ),
                        });
                    }
                }
                message => {
                    if let Message::Attitude { time_boot_ms, .. }
                    | Message::GlobalPositionInt { time_boot_ms, .. } = message
                    {
                        effects.extend(self.boot_time(&key, time_boot_ms, received_us));
                    }
                    if let Message::Attitude { yaw, .. } = message
                        && yaw.is_finite()
                        && let Some(vehicle) = self.vehicles.get_mut(&key)
                    {
                        vehicle.heading =
                            Some((f64::from(yaw).to_degrees().rem_euclid(360.0), received_us));
                    }
                    self.note_home(&key, &message, received_us);
                    if let Some(vehicle) = self.vehicles.get_mut(&key) {
                        vehicle.note_terrain(&message, received_us);
                        for producer in &self.producers {
                            producer.consume(
                                decoded.component_id,
                                &message,
                                received_us,
                                &mut vehicle.pending,
                            );
                        }
                        vehicle.add_above_ground(received_us);
                    }
                }
            }
        }
        effects
    }

    /// The autopilot at `key` has the UID of one heard earlier at another
    /// system id on the same link and peer: its system id changed (a
    /// parameter write, such as a snapshot's), so the old address is let
    /// go. Left, it would go stale and then offline under the same vehicle
    /// id while the vehicle flies on at its new address (ADR-0004: the
    /// address updates and the VID is kept by UID).
    fn moved(&mut self, key: &(String, String, u8)) {
        let Some(uid) = self
            .vehicles
            .get(key)
            .and_then(|v| v.observation.uid.clone())
        else {
            return;
        };
        let old: Vec<(String, String, u8)> = self
            .vehicles
            .iter()
            .filter(|(other, v)| {
                *other != key
                    && other.0 == key.0
                    && other.1 == key.1
                    && v.observation.uid.as_deref() == Some(uid.as_str())
            })
            .map(|(other, _)| other.clone())
            .collect();
        for address in old {
            let Some(gone) = self.vehicles.remove(&address) else {
                continue;
            };
            if let Some(vehicle) = self.vehicles.get_mut(key) {
                for alias in gone.observation.aliases {
                    if alias != vehicle.observation.id
                        && !vehicle.observation.aliases.contains(&alias)
                    {
                        vehicle.observation.aliases.push(alias);
                    }
                }
            }
            for command in &mut self.commands {
                if command.key == address {
                    command.key = key.clone();
                }
            }
        }
    }

    /// A component that is not an autopilot, such as a camera or a
    /// companion computer, joins the vehicle at its address.
    fn companion(&mut self, key: &(String, String, u8), component_id: u8) -> Option<Effect> {
        let vehicle = self.vehicles.get_mut(key)?;
        if vehicle.observation.component_ids.contains(&component_id) {
            return None;
        }
        vehicle.observation.component_ids.push(component_id);
        vehicle.observation.component_ids.sort_unstable();
        Some(Effect::Vehicle(Box::new(vehicle.observation.clone())))
    }

    /// A heartbeat: a vehicle's, or something else's. Ground stations,
    /// including ourselves looped back, are not vehicles.
    fn any_heartbeat(
        &mut self,
        key: &(String, String, u8),
        component_id: u8,
        beat: &Beat,
        now_us: u64,
        processed_us: u64,
    ) -> Vec<Effect> {
        if beat.autopilot == "Invalid" || beat.mav_type == "Gcs" {
            self.not_a_vehicle(key, component_id, &beat.mav_type, now_us)
        } else {
            self.heartbeat(key, component_id, beat, now_us, processed_us)
        }
    }

    /// An `AUTOPILOT_VERSION`: the vehicle's UID and firmware.
    fn version(
        &mut self,
        key: &(String, String, u8),
        uid: u64,
        uid2: [u8; 18],
        versions: (u32, [u8; 8]),
    ) -> Option<Effect> {
        let simulation = self.simulated.contains(&key.0).then_some(key.0.as_str());
        let vehicle = self.vehicles.get_mut(key)?;
        if !identify(vehicle, uid, uid2, versions, simulation) {
            return None;
        }
        self.moved(key);
        self.vehicles
            .get(key)
            .map(|vehicle| Effect::Vehicle(Box::new(vehicle.observation.clone())))
    }

    /// A heartbeat from something that is not a vehicle: ourselves looped
    /// back, another ground station, or a companion such as a camera.
    fn not_a_vehicle(
        &mut self,
        key: &(String, String, u8),
        component_id: u8,
        mav_type: &str,
        now_us: u64,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        if mav_type == "Gcs" {
            self.stations.insert(key.clone(), now_us);
        } else {
            effects.extend(self.companion(key, component_id));
        }
        if mav_type == "Camera" {
            self.cameras
                .heard((key.0.clone(), key.1.clone(), key.2, component_id));
            effects.extend(self.ask_cameras(now_us));
        }
        effects
    }

    /// A camera's answer: one of its streams, for its vehicle.
    /// A message, or a part of a long one: whole messages come out; parts
    /// wait for the last (one shorter than its 50 bytes), or two seconds.
    fn status_text(
        &mut self,
        key: &(String, String, u8),
        component_id: u8,
        message: Message,
        now_us: u64,
    ) -> Option<Effect> {
        const PARTS_US: u64 = 2_000_000;
        let Message::StatusText {
            severity,
            text,
            id,
            chunk_seq,
            full,
        } = message
        else {
            return None;
        };
        let vehicle_id = self.vehicles.get(key)?.observation.id.clone();
        self.texts
            .retain(|_, (_, at)| now_us.saturating_sub(*at) < PARTS_US);
        let text = if id == 0 {
            text
        } else {
            let part = (key.0.clone(), key.1.clone(), key.2, component_id, id);
            let (parts, at) = self.texts.entry(part.clone()).or_default();
            parts.insert(chunk_seq, text);
            *at = now_us;
            if full && parts.len() < 8 {
                return None;
            }
            let (parts, _) = self.texts.remove(&part)?;
            parts.into_values().collect()
        };
        let text = text.trim().to_owned();
        if text.is_empty() {
            return None;
        }
        Some(Effect::StatusText {
            vehicle_id,
            component_id,
            severity: severity.min(7),
            text,
        })
    }

    fn video_stream(
        &mut self,
        key: &(String, String, u8),
        component_id: u8,
        stream_id: u8,
        kind: u8,
        uri: String,
    ) -> Option<Effect> {
        let vehicle = self.vehicles.get(key)?;
        self.cameras
            .answered(&(key.0.clone(), key.1.clone(), key.2, component_id));
        Some(Effect::VideoStream {
            vehicle_id: vehicle.observation.id.clone(),
            component_id,
            stream_id,
            kind,
            uri,
        })
    }

    /// Asks the cameras that are due for their video streams.
    fn ask_cameras(&mut self, now_us: u64) -> Vec<Effect> {
        let gcs = self.config.gcs_system_id;
        let vehicles = &self.vehicles;
        let due = self.cameras.due(now_us, |(link, peer, system, _)| {
            vehicles.contains_key(&(link.clone(), peer.clone(), *system))
        });
        due.into_iter()
            .map(|(link_id, peer, system, component)| {
                let sequence = self.next_sequence();
                Effect::Send {
                    link_id,
                    peer,
                    bytes: crate::sim::request_message_frame(
                        gcs,
                        system,
                        component,
                        sequence,
                        crate::camera::VIDEO_STREAM_INFORMATION,
                        0.0,
                    ),
                }
            })
            .collect()
    }

    /// A parameter or mission protocol message from a vehicle.
    fn transfer(
        &mut self,
        key: &(String, String, u8),
        message: Message,
        received_us: u64,
        processed_us: u64,
    ) -> Vec<Effect> {
        match message {
            Message::ParamValue {
                name,
                value,
                param_type,
                count,
                index,
            } => {
                // PX4 sends an integer's bits, not its number.
                let value = if self.is_px4(key) {
                    crate::transfers::from_bytewise(value, param_type)
                } else {
                    value
                };
                let param = Param {
                    name,
                    value,
                    kind: param_type,
                };
                self.param_value(key, param, count, index, received_us)
            }
            // An answer to our FTP (S74, S121).
            Message::Ftp {
                target_system,
                payload,
                ..
            } if target_system == self.config.gcs_system_id => {
                let answer = crate::transfers::ftp::read(&payload);
                let Some(output) = self
                    .vehicles
                    .get_mut(key)
                    .map(|v| v.ftp.answer(&answer, received_us, processed_us))
                else {
                    return Vec::new();
                };
                self.ftp_output(key, output, processed_us)
            }
            Message::MissionCount {
                count,
                mission_type,
            } => {
                let step = self
                    .downloading(key)
                    .filter(|p| p.accepts(received_us) && p.protocol.kind == mission_type)
                    .map(|p| p.protocol.count(count, processed_us));
                self.download_step(key, step)
            }
            Message::MissionItem(item) => {
                let kind = item.mission_type;
                let step = self
                    .downloading(key)
                    .filter(|p| p.accepts(received_us) && p.protocol.kind == kind)
                    .map(|p| p.protocol.item(item, processed_us));
                self.download_step(key, step)
            }
            Message::MissionRequest { seq, mission_type } => {
                let step = self
                    .uploading(key)
                    .filter(|p| p.accepts(received_us) && p.protocol.kind == mission_type)
                    .map(|p| p.protocol.request(seq, processed_us));
                self.upload_step(key, step)
            }
            Message::MissionAck {
                result,
                mission_type,
            } => {
                // ArduPilot NACKs a duplicate item with INVALID_SEQUENCE
                // while continuing to request the next one. Aborting here
                // leaves a partial mission on the vehicle. Keep the existing
                // retry deadline: no next request still fails by timeout.
                if result == 13
                    && self.vehicles.get(key).is_some_and(|vehicle| {
                        vehicle.mission_upload.is_some()
                            && vehicle
                                .observation
                                .autopilot
                                .eq_ignore_ascii_case("ardupilotmega")
                    })
                {
                    return Vec::new();
                }
                let step = self
                    .uploading(key)
                    .filter(|p| p.accepts(received_us) && p.protocol.kind == mission_type)
                    .map(|p| p.protocol.ack(result));
                if step.is_some() {
                    self.upload_step(key, step)
                } else {
                    let step = self
                        .downloading(key)
                        .filter(|p| {
                            result != 0 && p.accepts(received_us) && p.protocol.kind == mission_type
                        })
                        .map(|_| {
                            Step::Failed(Failure::Rejected(
                                crate::transfers::mission_result_text(result).to_owned(),
                            ))
                        });
                    self.download_step(key, step)
                }
            }
            _ => Vec::new(),
        }
    }

    /// One mode of a vehicle's own list. Once every index has come, the
    /// list goes to the engine, when it differs from the last one sent.
    fn available_mode(
        &mut self,
        key: &(String, String, u8),
        number_modes: u8,
        mode_index: u8,
        mode: AvailableMode,
    ) -> Vec<Effect> {
        let Some(vehicle) = self.vehicles.get_mut(key) else {
            return Vec::new();
        };
        if number_modes == 0 || mode_index == 0 || mode_index > number_modes {
            return Vec::new();
        }
        vehicle.modes_heard.insert(mode_index, mode);
        if vehicle.modes_heard.len() < usize::from(number_modes) {
            return Vec::new();
        }
        let modes: Vec<AvailableMode> = std::mem::take(&mut vehicle.modes_heard)
            .into_values()
            .collect();
        if modes == vehicle.modes {
            return Vec::new();
        }
        vehicle.modes.clone_from(&modes);
        vec![Effect::Modes {
            vehicle_id: vehicle.observation.id.clone(),
            modes,
        }]
    }

    /// The vehicle says its list of modes changed (`Q_ENABLE` set, a script
    /// loaded): ask for it again. The first count heard is only noted, as
    /// the list was asked for with the first heartbeat.
    fn modes_changed(&mut self, key: &(String, String, u8), seq: u8) -> Vec<Effect> {
        let gcs = self.config.gcs_system_id;
        let sequence = self.next_sequence();
        let Some(vehicle) = self.vehicles.get_mut(key) else {
            return Vec::new();
        };
        if vehicle.modes_seq == Some(seq) {
            return Vec::new();
        }
        let first = vehicle.modes_seq.is_none();
        vehicle.modes_seq = Some(seq);
        vehicle.modes_heard.clear();
        if first {
            return Vec::new();
        }
        vec![Effect::Send {
            link_id: vehicle.observation.link_id.clone(),
            peer: vehicle.observation.peer.clone(),
            bytes: crate::sim::request_message_frame(
                gcs,
                vehicle.observation.system_id,
                vehicle.autopilot_component,
                sequence,
                AVAILABLE_MODES,
                0.0,
            ),
        }]
    }

    fn heartbeat(
        &mut self,
        key: &(String, String, u8),
        component_id: u8,
        beat: &Beat,
        now_us: u64,
        processed_us: u64,
    ) -> Vec<Effect> {
        let gcs = self.config.gcs_system_id;
        let Beat {
            mav_type,
            autopilot,
            armed,
            custom_mode,
        } = beat;
        let (mav_type, autopilot, armed) = (mav_type.clone(), autopilot.clone(), *armed);
        let table_mode = crate::modes::Family::for_vehicle(&autopilot, &mav_type)
            .and_then(|family| family.name(*custom_mode))
            .map(str::to_owned);
        let mut effects = Vec::new();
        let sequence = self.next_sequence();
        let new = !self.vehicles.contains_key(key);
        let vehicle = self
            .vehicles
            .entry(key.clone())
            .or_insert_with(|| Vehicle::new(key, component_id, &autopilot, &mav_type, now_us));
        // The table's name, else the vehicle's own for a mode it listed (a
        // scripted one).
        let mode = table_mode.or_else(|| {
            crate::modes::Family::listed_name(&vehicle.modes, *custom_mode).map(str::to_owned)
        });
        vehicle.observation.last_heartbeat_us = vehicle.observation.last_heartbeat_us.max(now_us);
        let presence = presence_at(
            self.config,
            vehicle.observation.last_heartbeat_us,
            processed_us,
        );
        if presence == Presence::Online && vehicle.observation.presence != Presence::Online {
            // Back after a silence: its streams get a settle window before
            // they are judged.
            vehicle.online_since_us = now_us;
        }
        let mut changed = new || vehicle.observation.presence != presence;
        vehicle.observation.presence = presence;
        if vehicle.observation.vehicle_type != mav_type
            || vehicle.observation.autopilot != autopilot
        {
            vehicle.observation.vehicle_type = mav_type;
            vehicle.observation.autopilot = autopilot;
            changed = true;
        }
        if !vehicle.observation.component_ids.contains(&component_id) {
            vehicle.observation.component_ids.push(component_id);
            vehicle.observation.component_ids.sort_unstable();
            changed = true;
        }
        if vehicle.observation.armed != armed || vehicle.observation.mode != mode {
            vehicle.observation.armed = armed;
            vehicle.observation.mode = mode;
            changed = true;
        }
        if changed {
            effects.push(Effect::Vehicle(Box::new(vehicle.observation.clone())));
        }
        // The whole table once, when the vehicle first appears; after that
        // only rows that go missing (see `reconcile_streams`).
        if new {
            for (message_id, interval_us) in STREAM_RATES {
                effects.push(Effect::Send {
                    link_id: vehicle.observation.link_id.clone(),
                    peer: vehicle.observation.peer.clone(),
                    bytes: set_message_interval_frame(
                        gcs,
                        vehicle.observation.system_id,
                        vehicle.autopilot_component,
                        sequence,
                        message_id,
                        interval_us,
                    ),
                });
            }
        }
        // Its own list of modes, for the command list: ArduPilot 4.6 and PX4
        // 1.15 onwards answer, older firmware says nothing and the table
        // serves.
        if new {
            effects.push(Effect::Send {
                link_id: vehicle.observation.link_id.clone(),
                peer: vehicle.observation.peer.clone(),
                bytes: crate::sim::request_message_frame(
                    gcs,
                    vehicle.observation.system_id,
                    vehicle.autopilot_component,
                    sequence,
                    AVAILABLE_MODES,
                    0.0,
                ),
            });
        }
        if vehicle.observation.uid.is_none() && vehicle.version_requests == 0 {
            vehicle.version_requests = 1;
            vehicle.last_request_us = now_us;
            effects.push(Effect::Send {
                link_id: vehicle.observation.link_id.clone(),
                peer: vehicle.observation.peer.clone(),
                bytes: request_autopilot_version_frame(
                    gcs,
                    vehicle.observation.system_id,
                    vehicle.autopilot_component,
                    sequence,
                ),
            });
        }
        effects
    }

    /// Advances time: presence transitions, peer expiry and version retries.
    pub fn tick(&mut self, now_us: u64) -> Vec<Effect> {
        let config = self.config;
        self.peers
            .retain(|_, last| now_us.saturating_sub(*last) <= config.peer_expiry_us);
        let mut effects = Vec::new();
        effects.extend(self.ask_cameras(now_us));
        let mut sends = Vec::new();
        let mut stream_requests = Vec::new();
        for vehicle in self.vehicles.values_mut() {
            // A paused simulator's vehicle stays as it was until it resumes.
            let paused = self.paused.contains(&vehicle.observation.link_id);
            let presence = if paused {
                vehicle.observation.presence
            } else {
                presence_at(config, vehicle.observation.last_heartbeat_us, now_us)
            };
            if presence != vehicle.observation.presence {
                vehicle.observation.presence = presence;
                effects.push(Effect::Vehicle(Box::new(vehicle.observation.clone())));
            }
            let produced = std::mem::take(&mut vehicle.pending);
            if !produced.positions.is_empty() || !produced.items.is_empty() {
                let mut telemetry = VehicleTelemetry {
                    vehicle_id: vehicle.observation.id.clone(),
                    ..VehicleTelemetry::default()
                };
                // Newest wins: several frames of one kind in a tick coalesce.
                telemetry.merge(&VehicleTelemetry {
                    vehicle_id: String::new(),
                    positions: produced.positions,
                    items: produced.items,
                });
                effects.push(Effect::Telemetry(telemetry));
            }
            let retry = vehicle.observation.uid.is_none()
                && !paused
                && presence == Presence::Online
                && vehicle.version_requests < config.version_attempts
                && now_us.saturating_sub(vehicle.last_request_us) >= config.version_retry_us;
            if retry {
                vehicle.version_requests += 1;
                vehicle.last_request_us = now_us;
                sends.push((
                    vehicle.observation.link_id.clone(),
                    vehicle.observation.peer.clone(),
                    vehicle.observation.system_id,
                    vehicle.autopilot_component,
                ));
            }
            if presence == Presence::Online && !paused {
                for (message_id, interval_us) in missing_streams(vehicle, &config, now_us) {
                    stream_requests.push((
                        vehicle.observation.link_id.clone(),
                        vehicle.observation.peer.clone(),
                        vehicle.observation.system_id,
                        vehicle.autopilot_component,
                        message_id,
                        interval_us,
                    ));
                }
            }
        }
        for (link_id, peer, system_id, component) in sends {
            let sequence = self.next_sequence();
            effects.push(Effect::Send {
                link_id,
                peer,
                bytes: request_autopilot_version_frame(
                    config.gcs_system_id,
                    system_id,
                    component,
                    sequence,
                ),
            });
        }
        for (link_id, peer, system_id, component, message_id, interval_us) in stream_requests {
            let sequence = self.next_sequence();
            effects.push(Effect::Send {
                link_id,
                peer,
                bytes: set_message_interval_frame(
                    config.gcs_system_id,
                    system_id,
                    component,
                    sequence,
                    message_id,
                    interval_us,
                ),
            });
        }
        effects.extend(self.retry_commands(now_us));
        effects.extend(self.tick_transfers(now_us));
        effects.extend(self.send_terrain(now_us));
        effects.extend(self.unanswered_checks(now_us));
        effects
    }

    /// Asks a vehicle for its own terrain height at a point
    /// (`TERRAIN_CHECK`, S119). Its `TERRAIN_REPORT` for that point comes
    /// back as [`Effect::TerrainChecked`]; none within five seconds, as
    /// one with no reply.
    pub fn terrain_check(
        &mut self,
        vehicle_id: &str,
        lat: i32,
        lon: i32,
        now_us: u64,
    ) -> Vec<Effect> {
        let Some(key) = self.find(vehicle_id) else {
            return vec![Effect::TerrainChecked {
                vehicle_id: vehicle_id.to_owned(),
                lat,
                lon,
                height: None,
            }];
        };
        // Its own id, which reports carry, even when asked by an alias.
        let id = self
            .vehicles
            .get(&key)
            .map_or_else(|| vehicle_id.to_owned(), |v| v.observation.id.clone());
        let (link_id, peer, _) = key;
        self.terrain_checks
            .entry(id)
            .or_default()
            .push((lat, lon, now_us));
        let sequence = self.next_sequence();
        vec![Effect::Send {
            link_id,
            peer,
            bytes: crate::sim::terrain_check_frame(self.config.gcs_system_id, sequence, lat, lon),
        }]
    }

    /// Whether a report at this point answers a check asked of this
    /// vehicle; the check is done if so.
    fn checked(&mut self, key: &(String, String, u8), lat: i32, lon: i32) -> bool {
        let Some(vehicle) = self.vehicles.get(key) else {
            return false;
        };
        let Some(checks) = self.terrain_checks.get_mut(&vehicle.observation.id) else {
            return false;
        };
        let Some(at) = checks.iter().position(|&(a, b, _)| a == lat && b == lon) else {
            return false;
        };
        checks.remove(at);
        true
    }

    /// Checks with no report within five seconds.
    fn unanswered_checks(&mut self, now_us: u64) -> Vec<Effect> {
        let mut effects = Vec::new();
        for (vehicle_id, checks) in &mut self.terrain_checks {
            checks.retain(|&(lat, lon, asked_us)| {
                let late = now_us.saturating_sub(asked_us) > 5_000_000;
                if late {
                    effects.push(Effect::TerrainChecked {
                        vehicle_id: vehicle_id.clone(),
                        lat,
                        lon,
                        height: None,
                    });
                }
                !late
            });
        }
        self.terrain_checks.retain(|_, c| !c.is_empty());
        effects
    }

    /// Queues a block's terrain answers for a vehicle (S118), keeping
    /// those queued for it only if `mask`, its latest request, still asks
    /// for them; the tick sends them at `per_second`.
    pub fn terrain_data(
        &mut self,
        vehicle_id: &str,
        block: crate::terrain::Block,
        groups: Vec<crate::terrain::Group>,
        mask: u64,
        per_second: u8,
        now_us: u64,
    ) -> Vec<Effect> {
        self.terrain
            .queue(vehicle_id, block, groups, mask, per_second, now_us);
        self.send_terrain(now_us)
    }

    /// The terrain answers due now, to each vehicle's current address. A
    /// read-only link's transport drops them, as it drops every send.
    fn send_terrain(&mut self, now_us: u64) -> Vec<Effect> {
        let mut effects = Vec::new();
        for (vehicle_id, block, group) in self.terrain.due(now_us) {
            let Some((link_id, peer, _)) = self.find(&vehicle_id) else {
                self.terrain.forget(&vehicle_id);
                continue;
            };
            let sequence = self.next_sequence();
            effects.push(Effect::Send {
                link_id,
                peer,
                bytes: crate::sim::terrain_data_frame(
                    self.config.gcs_system_id,
                    sequence,
                    block,
                    group,
                ),
            });
        }
        effects
    }

    /// Sends a command to a vehicle's current address and waits for its
    /// acknowledgement, retrying (`tick`) and giving up after the last try.
    /// A vehicle with no address is `NotSent` at once.
    pub fn command(&mut self, order: CommandOrder, now_us: u64) -> Vec<Effect> {
        let not_sent = |order: CommandOrder, reason: &str| {
            vec![Effect::CommandDone {
                receipt_id: order.receipt_id,
                outcome: CommandOutcome::NotSent(reason.into()),
                attempts: 0,
            }]
        };
        let Some((key, vehicle)) = self.vehicles.iter().find(|(_, v)| {
            v.observation.id == order.vehicle_id
                || v.observation.aliases.contains(&order.vehicle_id)
        }) else {
            return not_sent(order, "this vehicle has no address in this session");
        };
        let family = crate::modes::Family::for_vehicle(
            &vehicle.observation.autopilot,
            &vehicle.observation.vehicle_type,
        );
        let (command, mut wire) = match crate::commands::encode_listed(
            &order.command,
            family,
            &vehicle.modes,
            order.force,
        ) {
            Ok(encoded) => encoded,
            Err(reason) => return not_sent(order, &reason),
        };
        // PX4's take-off: its altitude is above mean sea level, not home,
        // and a yaw or place given as 0 would be used, so they are NaN.
        if family == Some(crate::modes::Family::Px4)
            && command == MAV_CMD_NAV_TAKEOFF
            && let crate::commands::Wire::Long(params) = &mut wire
        {
            let Some(home_m) = vehicle.home_amsl_m else {
                return not_sent(order, "its home altitude is not known yet");
            };
            params[3] = f32::NAN;
            params[4] = f32::NAN;
            params[5] = f32::NAN;
            params[6] += home_m;
        }
        let follow_up = if family == Some(crate::modes::Family::Sub)
            && (self.simulated.contains(&key.0)
                || matches!(
                    &order.command,
                    uxduck_core::domain::command::Command::GoTo {
                        heading: Some(_),
                        ..
                    }
                )) {
            match vehicle.go_to_heading(&order.command, now_us) {
                Ok(heading) => heading,
                Err(why) => return not_sent(order, why),
            }
        } else {
            None
        };
        // One command of a kind at a time per vehicle: an ack cannot say
        // which of two identical commands it answers.
        if self.commands.iter().any(|p| {
            p.key == *key
                && (p.command == command
                    || p.first_command == command
                    || p.follow_up.is_some_and(|(next, _)| next == command)
                    || follow_up.is_some_and(|(next, _)| next == p.command))
        }) {
            return not_sent(order, "the same command is still waiting for its answer");
        }
        let pending = PendingCommand {
            order,
            key: key.clone(),
            component: vehicle.autopilot_component,
            command,
            wire,
            attempts: 0,
            sent_us: now_us,
            issued_us: now_us,
            follow_up,
            first_command: command,
            completed_attempts: 0,
        };
        // A mode change or disarm takes over from any
        // queued automatic heading. Do not rotate later after that takeover.
        let takes_over = matches!(
            pending.order.command,
            uxduck_core::domain::command::Command::SetMode { .. }
                | uxduck_core::domain::command::Command::Disarm
                | uxduck_core::domain::command::Command::Rtl
                | uxduck_core::domain::command::Command::Land
        );
        let mut effects = if takes_over {
            self.cancel_heading(&pending.key)
        } else {
            Vec::new()
        };
        self.commands.push(pending);
        let index = self.commands.len() - 1;
        effects.push(self.transmit(index, now_us));
        effects
    }

    /// Cancel the automatic heading when an explicit mode or disarm command
    /// takes over. The destination may already have been accepted.
    fn cancel_heading(&mut self, key: &(String, String, u8)) -> Vec<Effect> {
        let mut effects = Vec::new();
        self.commands.retain_mut(|previous| {
            if previous.key != *key {
                return true;
            }
            previous.follow_up = None;
            if previous.completed_attempts == 0 {
                return true;
            }
            effects.push(Effect::CommandDone {
                receipt_id: previous.order.receipt_id.clone(),
                outcome: CommandOutcome::NotSent(
                    "destination accepted; heading superseded by another command".into(),
                ),
                attempts: previous
                    .completed_attempts
                    .saturating_add(previous.attempts),
            });
            false
        });
        effects
    }

    fn transmit(&mut self, index: usize, now_us: u64) -> Effect {
        let sequence = self.next_sequence();
        let gcs = self.config.gcs_system_id;
        let pending = &mut self.commands[index];
        let confirmation = pending.attempts;
        pending.attempts += 1;
        pending.sent_us = now_us;
        let (link_id, peer, system_id) = pending.key.clone();
        let bytes = match pending.wire {
            crate::commands::Wire::Long(params) => crate::sim::command_long_frame(
                gcs,
                system_id,
                pending.component,
                sequence,
                pending.command,
                params,
                confirmation,
            ),
            crate::commands::Wire::Int {
                frame,
                params,
                x,
                y,
                z,
            } => crate::sim::command_int_frame(
                gcs,
                system_id,
                pending.component,
                sequence,
                pending.command,
                crate::sim::IntPlace {
                    frame,
                    params,
                    x,
                    y,
                    z,
                },
            ),
        };
        Effect::Send {
            link_id,
            peer,
            bytes,
        }
    }

    fn acknowledged(
        &mut self,
        key: &(String, String, u8),
        command: u16,
        result: u8,
        now_us: u64,
    ) -> Vec<Effect> {
        let Some(index) = self
            .commands
            .iter()
            .position(|p| p.key == *key && p.command == command && now_us >= p.issued_us)
        else {
            return Vec::new();
        };
        // In progress: the vehicle is working on it; keep waiting.
        if result == 5 {
            self.commands[index].sent_us = self.commands[index].sent_us.max(now_us);
            return Vec::new();
        }
        if result == 0
            && let Some((command, wire)) = self.commands[index].follow_up.take()
        {
            let pending = &mut self.commands[index];
            pending.completed_attempts = pending.attempts;
            pending.attempts = 0;
            pending.command = command;
            pending.wire = wire;
            pending.issued_us = now_us;
            return vec![self.transmit(index, now_us)];
        }
        let pending = self.commands.remove(index);
        vec![Effect::CommandDone {
            receipt_id: pending.order.receipt_id,
            outcome: if result == 0 {
                CommandOutcome::Accepted
            } else {
                let reason = crate::commands::result_text(result);
                CommandOutcome::Rejected(if pending.completed_attempts > 0 {
                    format!("destination accepted; heading refused: {reason}")
                } else {
                    reason.into()
                })
            },
            attempts: pending.completed_attempts.saturating_add(pending.attempts),
        }]
    }

    fn retry_commands(&mut self, now_us: u64) -> Vec<Effect> {
        let config = self.config;
        let mut effects = Vec::new();
        let mut index = 0;
        while index < self.commands.len() {
            let pending = &self.commands[index];
            if now_us.saturating_sub(pending.sent_us) < config.command_timeout_us {
                index += 1;
            } else if pending.attempts < config.command_attempts {
                effects.push(self.transmit(index, now_us));
                index += 1;
            } else {
                let pending = self.commands.remove(index);
                effects.push(Effect::CommandDone {
                    receipt_id: pending.order.receipt_id,
                    outcome: if pending.completed_attempts > 0 {
                        CommandOutcome::Rejected(
                            "destination accepted; heading command timed out".into(),
                        )
                    } else {
                        CommandOutcome::Timeout
                    },
                    attempts: pending.completed_attempts.saturating_add(pending.attempts),
                });
            }
        }
        effects
    }

    /// Frames what a protocol wants sent, to the vehicle's autopilot.
    fn outgoing(&mut self, key: &(String, String, u8), outs: Vec<Out>) -> Vec<Effect> {
        let Some(component) = self.vehicles.get(key).map(|v| v.autopilot_component) else {
            return Vec::new();
        };
        let (link_id, peer, system_id) = key.clone();
        let gcs = self.config.gcs_system_id;
        let route = Route::to_vehicle(gcs, system_id, component);
        let px4 = self.is_px4(key);
        outs.into_iter()
            .map(|out| {
                let sequence = self.next_sequence();
                let bytes = match out {
                    Out::ParamRequestList => {
                        crate::sim::param_request_list_frame(gcs, system_id, component, sequence)
                    }
                    Out::ParamRequestRead(index) => crate::sim::param_request_read_frame(
                        gcs, system_id, component, sequence, index,
                    ),
                    Out::ParamSet(param) => crate::sim::param_set_frame(
                        gcs,
                        system_id,
                        component,
                        sequence,
                        &param.name,
                        if px4 {
                            crate::transfers::to_bytewise(param.value, param.kind)
                        } else {
                            param.value
                        },
                        param.kind,
                    ),
                    Out::Ftp(request) => crate::sim::ftp_frame(
                        (gcs, crate::GCS_COMPONENT_ID),
                        (system_id, component),
                        sequence,
                        crate::transfers::ftp::payload(&request),
                    ),
                    Out::MissionRequestList(kind) => crate::sim::mission_request_list_frame(
                        gcs, system_id, component, sequence, kind,
                    ),
                    Out::MissionCount(count, kind) => {
                        crate::sim::mission_count_frame(route, sequence, count, kind)
                    }
                    Out::MissionRequest(seq, kind) => {
                        crate::sim::mission_request_frame(route, sequence, seq, kind)
                    }
                    Out::MissionItem(item) => {
                        crate::sim::mission_item_frame(route, sequence, &item)
                    }
                    Out::MissionAck(result, kind) => {
                        crate::sim::mission_ack_frame(route, sequence, result, kind)
                    }
                };
                Effect::Send {
                    link_id: link_id.clone(),
                    peer: peer.clone(),
                    bytes,
                }
            })
            .collect()
    }

    fn find(&self, vehicle_id: &str) -> Option<(String, String, u8)> {
        self.vehicles
            .iter()
            .find(|(_, v)| {
                v.observation.id == vehicle_id
                    || v.observation.aliases.iter().any(|a| a == vehicle_id)
            })
            .map(|(key, _)| key.clone())
    }

    /// Starts downloading every parameter. A download already running
    /// starts again.
    pub fn download_parameters(&mut self, vehicle_id: &str, now_us: u64) -> Vec<Effect> {
        let Some(key) = self.find(vehicle_id) else {
            return vec![Effect::Parameters {
                vehicle_id: vehicle_id.to_owned(),
                result: Err(Failure::NotSent(
                    "this vehicle has no address in this session".into(),
                )),
                defaults: BTreeMap::new(),
            }];
        };
        // By FTP first, with defaults; the list if that is refused.
        let Some(output) = self.vehicles.get_mut(&key).map(|vehicle| {
            vehicle.params = None;
            vehicle.ftp.parameters(now_us)
        }) else {
            return Vec::new();
        };
        self.ftp_output(&key, output, now_us)
    }

    /// FTP gave up: the list instead, which every autopilot has.
    fn parameters_by_list(&mut self, key: &(String, String, u8), now_us: u64) -> Vec<Effect> {
        let (download, outs) = ParamDownload::start(now_us);
        if let Some(vehicle) = self.vehicles.get_mut(key) {
            vehicle.params_by_file = false;
            vehicle.params = Some(PendingTransfer::new(download, now_us));
        }
        self.outgoing(key, outs)
    }

    /// A file job for a vehicle (S121), answered by `Effect::File` events
    /// under `id`, the last of them `Done`.
    pub fn file(&mut self, vehicle_id: &str, id: &str, op: files::Op, now_us: u64) -> Vec<Effect> {
        let Some(key) = self.find(vehicle_id) else {
            return vec![Effect::File {
                vehicle_id: vehicle_id.to_owned(),
                event: files::Event::Done {
                    id: id.to_owned(),
                    result: Err(Failure::NotSent(
                        "this vehicle has no address in this session".into(),
                    )),
                },
            }];
        };
        let Some(output) = self
            .vehicles
            .get_mut(&key)
            .map(|vehicle| vehicle.ftp.file(id, op, now_us))
        else {
            return Vec::new();
        };
        self.ftp_output(&key, output, now_us)
    }

    /// Stops a file job without reporting it; a file open on the vehicle
    /// is closed.
    pub fn cancel_file(&mut self, vehicle_id: &str, id: &str, now_us: u64) -> Vec<Effect> {
        let Some(key) = self.find(vehicle_id) else {
            return Vec::new();
        };
        let Some(output) = self
            .vehicles
            .get_mut(&key)
            .map(|vehicle| vehicle.ftp.cancel(id, now_us))
        else {
            return Vec::new();
        };
        self.ftp_output(&key, output, now_us)
    }

    /// What the FTP client decided: requests to send, and what they mean.
    fn ftp_output(
        &mut self,
        key: &(String, String, u8),
        output: files::Output,
        now_us: u64,
    ) -> Vec<Effect> {
        let mut effects = self.outgoing(key, output.send.into_iter().map(Out::Ftp).collect());
        for event in output.events {
            effects.extend(self.ftp_event(key, event, now_us));
        }
        effects
    }

    fn ftp_event(
        &mut self,
        key: &(String, String, u8),
        event: files::Event,
        now_us: u64,
    ) -> Vec<Effect> {
        let ardupilot = !self.is_px4(key);
        let Some(vehicle) = self.vehicles.get_mut(key) else {
            return Vec::new();
        };
        let vehicle_id = vehicle.observation.id.clone();
        match event {
            files::Event::Parameters(Ok(all)) => {
                vehicle.params_by_file = ardupilot;
                vehicle.rc_input.downloaded(all.iter().map(|(p, _)| p));
                let defaults = all
                    .iter()
                    .filter_map(|(param, default)| default.map(|d| (param.name.clone(), d)))
                    .collect();
                vec![Effect::Parameters {
                    vehicle_id,
                    result: Ok(all.into_iter().map(|(param, _)| param).collect()),
                    defaults,
                }]
            }
            // No FTP, or no parameter file: the list.
            files::Event::Parameters(Err(_)) => self.parameters_by_list(key, now_us),
            files::Event::ParamsWritten { writes, read_back } => {
                self.params_written(key, writes, read_back, now_us)
            }
            event => vec![Effect::File { vehicle_id, event }],
        }
    }

    /// Parameters written as one file, confirmed by reading the file back:
    /// each write is answered by the value read back. Any the read-back
    /// could not confirm go again by `PARAM_SET`, which ArduPilot answers
    /// one by one (ADR-0027 F13).
    fn params_written(
        &mut self,
        key: &(String, String, u8),
        writes: Vec<(String, Param)>,
        read_back: Result<Vec<crate::transfers::WithDefault>, String>,
        now_us: u64,
    ) -> Vec<Effect> {
        let Some(vehicle) = self.vehicles.get_mut(key) else {
            return Vec::new();
        };
        let vehicle_id = vehicle.observation.id.clone();
        let Ok(all) = read_back else {
            vehicle.params_by_file = false;
            return writes
                .into_iter()
                .flat_map(|(order_id, param)| self.write_by_message(key, order_id, param, now_us))
                .collect();
        };
        vehicle.rc_input.downloaded(all.iter().map(|(p, _)| p));
        let held: BTreeMap<&str, &Param> = all.iter().map(|(p, _)| (p.name.as_str(), p)).collect();
        let mut effects = Vec::new();
        let mut again = Vec::new();
        for (order_id, param) in writes {
            match held.get(param.name.as_str()) {
                Some(now) if same_value(f64::from(now.value), f64::from(param.value)) => {
                    effects.push(Effect::ParamWritten {
                        order_id,
                        result: Ok((*now).clone()),
                        attempts: 1,
                    });
                }
                Some(_) => again.push((order_id, param)),
                None => effects.push(Effect::ParamWritten {
                    order_id,
                    result: Err(Failure::Rejected(format!(
                        "the vehicle has no parameter {}",
                        param.name
                    ))),
                    attempts: 1,
                }),
            }
        }
        // Everything else it holds, with the defaults: other parameters
        // can change with the ones written.
        let defaults = all
            .iter()
            .filter_map(|(param, default)| default.map(|d| (param.name.clone(), d)))
            .collect();
        effects.push(Effect::Parameters {
            vehicle_id,
            result: Ok(all.into_iter().map(|(param, _)| param).collect()),
            defaults,
        });
        for (order_id, param) in again {
            effects.extend(self.write_by_message(key, order_id, param, now_us));
        }
        effects
    }

    /// Writes waiting since the last tick: one file when there are enough
    /// of them, `PARAM_SET` each otherwise.
    fn flush_batch(&mut self, key: &(String, String, u8), now_us: u64) -> Vec<Effect> {
        let Some(vehicle) = self.vehicles.get_mut(key) else {
            return Vec::new();
        };
        let batch = std::mem::take(&mut vehicle.batch);
        if batch.len() >= PARAM_FILE_WRITES && vehicle.params_by_file {
            let output = vehicle.ftp.write_parameters(batch, now_us);
            return self.ftp_output(key, output, now_us);
        }
        batch
            .into_iter()
            .flat_map(|(order_id, param)| self.write_by_message(key, order_id, param, now_us))
            .collect()
    }

    /// `PARAM_SET`, sent until the vehicle reports the parameter back.
    fn write_by_message(
        &mut self,
        key: &(String, String, u8),
        order_id: String,
        param: Param,
        now_us: u64,
    ) -> Vec<Effect> {
        let (write, outs) = ParamWrite::start(order_id, param, now_us);
        if let Some(vehicle) = self.vehicles.get_mut(key) {
            // A newer write of the same parameter replaces the older.
            vehicle
                .writes
                .retain(|w| w.protocol.param.name != write.param.name);
            vehicle.writes.push(PendingTransfer::new(write, now_us));
        }
        self.outgoing(key, outs)
    }

    /// Sets a parameter, answered by `ParamWritten` with what the vehicle
    /// reports back. `by_file`: with the others asked for before the next
    /// tick, as one parameter file when there are enough (S121).
    pub fn set_parameter(
        &mut self,
        order_id: &str,
        vehicle_id: &str,
        param: Param,
        by_file: bool,
        now_us: u64,
    ) -> Vec<Effect> {
        let Some(key) = self.find(vehicle_id) else {
            return vec![Effect::ParamWritten {
                order_id: order_id.to_owned(),
                result: Err(Failure::NotSent(
                    "this vehicle has no address in this session".into(),
                )),
                attempts: 0,
            }];
        };
        // Asked for by file, on a vehicle that takes parameters that way, a
        // write waits for the next tick (a tenth of a second), so writes
        // made together go together: as one file when there are enough
        // (ADR-0027 F13). Others go at once.
        if let Some(vehicle) = self.vehicles.get_mut(&key)
            && by_file
            && vehicle.params_by_file
        {
            vehicle
                .batch
                .retain(|(_, queued)| queued.name != param.name);
            vehicle.batch.push((order_id.to_owned(), param));
            return Vec::new();
        }
        self.write_by_message(&key, order_id.to_owned(), param, now_us)
    }

    /// Starts downloading the mission.
    pub fn download_mission(&mut self, vehicle_id: &str, now_us: u64) -> Vec<Effect> {
        self.download_mission_part(vehicle_id, 0, now_us)
    }

    /// Starts downloading one kind (S128): 0 the mission, 1 the fence, 2
    /// rally points; ends in `Mission` or `MissionPart`.
    pub fn download_mission_part(
        &mut self,
        vehicle_id: &str,
        kind: u8,
        now_us: u64,
    ) -> Vec<Effect> {
        let Some(key) = self.find(vehicle_id) else {
            return vec![part_effect(
                vehicle_id.to_owned(),
                kind,
                Err(Failure::NotSent(
                    "this vehicle has no address in this session".into(),
                )),
            )];
        };
        let busy = self
            .vehicles
            .get(&key)
            .is_some_and(|v| v.mission_upload.is_some());
        if busy {
            return vec![part_effect(
                vehicle_id.to_owned(),
                kind,
                Err(Failure::NotSent("an upload is in progress".into())),
            )];
        }
        let (download, outs) = MissionDownload::start_kind(now_us, kind);
        if let Some(vehicle) = self.vehicles.get_mut(&key) {
            vehicle.mission_download = Some(PendingTransfer::new(download, now_us));
        }
        self.outgoing(&key, outs)
    }

    /// Uploads a mission, answered by `MissionUploaded`.
    pub fn upload_mission(
        &mut self,
        order_id: &str,
        vehicle_id: &str,
        items: Vec<crate::decode::MissionItemInt>,
        now_us: u64,
    ) -> Vec<Effect> {
        self.upload_mission_part(order_id, vehicle_id, items, 0, now_us)
    }

    /// Uploads one kind (S128): 0 the mission, 1 the fence, 2 rally points;
    /// answered by `MissionUploaded`.
    pub fn upload_mission_part(
        &mut self,
        order_id: &str,
        vehicle_id: &str,
        items: Vec<crate::decode::MissionItemInt>,
        kind: u8,
        now_us: u64,
    ) -> Vec<Effect> {
        let not_sent = |why: &str| {
            vec![Effect::MissionUploaded {
                order_id: order_id.to_owned(),
                result: Err(Failure::NotSent(why.to_owned())),
            }]
        };
        let Some(key) = self.find(vehicle_id) else {
            return not_sent("this vehicle has no address in this session");
        };
        let busy = self
            .vehicles
            .get(&key)
            .is_some_and(|v| v.mission_upload.is_some() || v.mission_download.is_some());
        if busy {
            return not_sent("another mission transfer is in progress");
        }
        let (upload, outs) = MissionUpload::start_kind(order_id.to_owned(), items, now_us, kind);
        if let Some(vehicle) = self.vehicles.get_mut(&key) {
            vehicle.mission_upload = Some(PendingTransfer::new(upload, now_us));
        }
        self.outgoing(&key, outs)
    }

    /// Stops a transfer (S127). A mission transfer is ended with the
    /// protocol's cancel (`MAV_MISSION_OPERATION_CANCELLED`); a parameter
    /// download stops asking and keeps nothing. Ends as the transfer
    /// would, with a `Rejected("cancelled")`.
    pub fn cancel_transfer(&mut self, vehicle_id: &str, transfer: TransferKind) -> Vec<Effect> {
        const CANCELLED: u8 = 15;
        let Some(key) = self.find(vehicle_id) else {
            return Vec::new();
        };
        let Some(vehicle) = self.vehicles.get_mut(&key) else {
            return Vec::new();
        };
        let cancelled = || Failure::Rejected("cancelled".into());
        let vehicle_id = vehicle.observation.id.clone();
        match transfer {
            TransferKind::Parameters => {
                if vehicle.params.take().is_none() {
                    return Vec::new();
                }
                vec![Effect::Parameters {
                    vehicle_id,
                    result: Err(cancelled()),
                    defaults: BTreeMap::new(),
                }]
            }
            TransferKind::MissionDownload => {
                let Some(download) = vehicle.mission_download.take() else {
                    return Vec::new();
                };
                let kind = download.protocol.kind;
                let mut effects = self.outgoing(&key, vec![Out::MissionAck(CANCELLED, kind)]);
                effects.push(part_effect(vehicle_id, kind, Err(cancelled())));
                effects
            }
            TransferKind::MissionUpload => {
                let Some(upload) = vehicle.mission_upload.take() else {
                    return Vec::new();
                };
                let kind = upload.protocol.kind;
                let mut effects = self.outgoing(&key, vec![Out::MissionAck(CANCELLED, kind)]);
                effects.push(Effect::MissionUploaded {
                    order_id: upload.protocol.order_id,
                    result: Err(cancelled()),
                });
                effects
            }
        }
    }

    /// Remember the fused position and home's altitude above mean sea
    /// level, from the vehicle's altitudes above sea level and above home.
    fn note_home(&mut self, key: &(String, String, u8), message: &Message, now_us: u64) {
        if let Message::GlobalPositionInt {
            alt,
            relative_alt,
            lat,
            lon,
            ..
        } = *message
            && let Some(vehicle) = self.vehicles.get_mut(key)
        {
            #[allow(clippy::cast_precision_loss, reason = "millimetres")]
            let home_m = (i64::from(alt) - i64::from(relative_alt)) as f32 / 1000.0;
            vehicle.home_amsl_m = Some(home_m);
            if (-900_000_000..=900_000_000).contains(&lat)
                && (-1_800_000_000..=1_800_000_000).contains(&lon)
                && (lat != 0 || lon != 0)
            {
                vehicle.position = Some((f64::from(lat) / 1e7, f64::from(lon) / 1e7, now_us));
            }
        }
    }

    /// Whether the vehicle at this address runs PX4, which encodes
    /// parameters by their bits.
    fn is_px4(&self, key: &(String, String, u8)) -> bool {
        self.vehicles
            .get(key)
            .is_some_and(|v| v.observation.autopilot.eq_ignore_ascii_case("px4"))
    }

    fn param_value(
        &mut self,
        key: &(String, String, u8),
        param: Param,
        count: u16,
        index: u16,
        now_us: u64,
    ) -> Vec<Effect> {
        let station = self.station_heard(now_us);
        let Some(vehicle) = self.vehicles.get_mut(key) else {
            return Vec::new();
        };
        let vehicle_id = vehicle.observation.id.clone();
        vehicle.rc_input.note(&param);
        let mut effects = Vec::new();
        if let Some(position) = vehicle
            .writes
            .iter()
            .position(|w| w.protocol.param.name == param.name && w.accepts(now_us))
        {
            let write = vehicle.writes.remove(position);
            effects.push(Effect::ParamWritten {
                order_id: write.protocol.order_id.clone(),
                result: Ok(param.clone()),
                attempts: write.protocol.attempts(),
            });
        }
        if let Some(download) = vehicle.params.as_mut() {
            if !download.accepts(now_us) {
                return effects;
            }
            if let Some(all) = download.protocol.value(param.clone(), count, index, now_us) {
                vehicle.params = None;
                vehicle.rc_input.downloaded(all.iter());
                effects.push(Effect::Parameters {
                    vehicle_id,
                    result: Ok(all),
                    defaults: BTreeMap::new(),
                });
            } else if index != u16::MAX {
                let (received, total) = download.protocol.progress();
                effects.push(Effect::Progress {
                    vehicle_id,
                    transfer: TransferKind::Parameters,
                    done: as_u32(received),
                    total: total.map(u32::from),
                    attempt: download.protocol.attempt(),
                });
            }
            // Listed values wait for the end of the download; a single
            // one (a change) is reported now as well.
            if index != u16::MAX {
                return effects;
            }
        }
        effects.push(Effect::ParamValue {
            vehicle_id: vehicle.observation.id.clone(),
            param,
            station,
        });
        effects
    }

    fn downloading(
        &mut self,
        key: &(String, String, u8),
    ) -> Option<&mut PendingTransfer<MissionDownload>> {
        self.vehicles.get_mut(key)?.mission_download.as_mut()
    }

    fn uploading(
        &mut self,
        key: &(String, String, u8),
    ) -> Option<&mut PendingTransfer<MissionUpload>> {
        self.vehicles.get_mut(key)?.mission_upload.as_mut()
    }

    fn download_step(
        &mut self,
        key: &(String, String, u8),
        step: Option<Step<Vec<crate::decode::MissionItemInt>>>,
    ) -> Vec<Effect> {
        let Some(step) = step else {
            return Vec::new();
        };
        let Some(vehicle) = self.vehicles.get_mut(key) else {
            return Vec::new();
        };
        let vehicle_id = vehicle.observation.id.clone();
        match step {
            Step::Send(outs) => {
                let progress = vehicle
                    .mission_download
                    .as_ref()
                    .filter(|_| !outs.is_empty())
                    .map(|download| download.protocol.progress());
                let mut effects = self.outgoing(key, outs);
                if let Some((received, total, attempt)) = progress {
                    effects.push(Effect::Progress {
                        vehicle_id,
                        transfer: TransferKind::MissionDownload,
                        done: as_u32(received),
                        total: total.map(u32::from),
                        attempt,
                    });
                }
                effects
            }
            Step::Done(items) => {
                let kind = vehicle
                    .mission_download
                    .take()
                    .map_or(0, |d| d.protocol.kind);
                let mut effects = self.outgoing(key, vec![Out::MissionAck(0, kind)]);
                effects.push(part_effect(vehicle_id, kind, Ok(items)));
                effects
            }
            Step::Failed(failure) => {
                let kind = vehicle
                    .mission_download
                    .take()
                    .map_or(0, |d| d.protocol.kind);
                vec![part_effect(vehicle_id, kind, Err(failure))]
            }
        }
    }

    fn upload_step(&mut self, key: &(String, String, u8), step: Option<Step<()>>) -> Vec<Effect> {
        let Some(step) = step else {
            return Vec::new();
        };
        let Some(vehicle) = self.vehicles.get_mut(key) else {
            return Vec::new();
        };
        match step {
            Step::Send(outs) => {
                let vehicle_id = vehicle.observation.id.clone();
                let progress = vehicle
                    .mission_upload
                    .as_ref()
                    .filter(|_| !outs.is_empty())
                    .map(|upload| upload.protocol.progress());
                let mut effects = self.outgoing(key, outs);
                if let Some((sent, total, attempt)) = progress {
                    effects.push(Effect::Progress {
                        vehicle_id,
                        transfer: TransferKind::MissionUpload,
                        done: as_u32(sent),
                        total: Some(as_u32(total)),
                        attempt,
                    });
                }
                effects
            }
            Step::Done(()) | Step::Failed(_) => {
                let result = match step {
                    Step::Failed(failure) => Err(failure),
                    _ => Ok(()),
                };
                vehicle
                    .mission_upload
                    .take()
                    .map(|upload| Effect::MissionUploaded {
                        order_id: upload.protocol.order_id,
                        result,
                    })
                    .into_iter()
                    .collect()
            }
        }
    }

    /// Tracks the boot instant from `time_boot_ms`. A later instant means
    /// the autopilot restarted (ADR-0004 reboot detection): the whole
    /// stream table is sent again and the caller told, and transfers in
    /// flight are abandoned.
    fn boot_time(
        &mut self,
        key: &(String, String, u8),
        time_boot_ms: u32,
        now_us: u64,
    ) -> Vec<Effect> {
        let gcs = self.config.gcs_system_id;
        let Some(vehicle) = self.vehicles.get_mut(key) else {
            return Vec::new();
        };
        let estimate = now_us.saturating_sub(u64::from(time_boot_ms) * 1000);
        // Back by more than reordering explains: it restarted, however soon
        // after its last boot.
        let went_back = time_boot_ms.saturating_add(1000) < vehicle.last_time_boot_ms;
        // Its time since boot carrying on from where it was, however long
        // since it was last heard: it was paused, not restarted. A simulator
        // stops while the computer sleeps, or while its browser tab is in
        // the background, and the computer's clock does not (maintainer,
        // 2026-10-03: noise on waking). A restart that happened to come back
        // within seconds of its old time since boot is taken for one too.
        let carried_on =
            !went_back && time_boot_ms.saturating_sub(vehicle.last_time_boot_ms) < PAUSE_SLACK_MS;
        vehicle.last_time_boot_ms = time_boot_ms;
        let restarted = vehicle.boot_instant_us.is_some_and(|known| {
            went_back || (estimate > known + REBOOT_TOLERANCE_US && !carried_on)
        });
        match vehicle.boot_instant_us {
            None => {
                vehicle.boot_instant_us = Some(estimate);
                Vec::new()
            }
            // Delay only makes an estimate later, so the earliest is best.
            Some(known) if estimate < known && !restarted => {
                vehicle.boot_instant_us = Some(estimate);
                Vec::new()
            }
            // Paused: its boot moves on by the pause.
            Some(known) if estimate > known + REBOOT_TOLERANCE_US && !restarted => {
                vehicle.boot_instant_us = Some(estimate);
                Vec::new()
            }
            Some(_) if restarted => {
                vehicle.boot_instant_us = Some(estimate);
                vehicle.position = None;
                vehicle.heading = None;
                vehicle.params = None;
                vehicle.rc_input = crate::rc_input::RcInput::default();
                vehicle.rc_overridden = [false; 18];
                let lost_files = vehicle.ftp.restarted();
                vehicle.mission_download = None;
                for stream in vehicle.streams.values_mut() {
                    *stream = Stream::default();
                }
                vehicle.online_since_us = now_us;
                let mut effects = Vec::new();
                if let Some(upload) = vehicle.mission_upload.take() {
                    effects.push(Effect::MissionUploaded {
                        order_id: upload.protocol.order_id,
                        result: Err(Failure::Rejected("the vehicle restarted".into())),
                    });
                }
                let vehicle_id = vehicle.observation.id.clone();
                for event in lost_files {
                    match event {
                        // Sent again by PARAM_SET once the tick comes.
                        files::Event::ParamsWritten { writes, .. } => vehicle.batch.extend(writes),
                        event => effects.push(Effect::File {
                            vehicle_id: vehicle_id.clone(),
                            event,
                        }),
                    }
                }
                if vehicle.boot_announced {
                    effects.push(Effect::Booted {
                        vehicle_id: vehicle.observation.id.clone(),
                        boot_instant_us: estimate,
                        restarted: true,
                    });
                }
                let (link_id, peer, system_id) = key.clone();
                let component = vehicle.autopilot_component;
                for (message_id, interval_us) in STREAM_RATES {
                    let sequence = self.next_sequence();
                    effects.push(Effect::Send {
                        link_id: link_id.clone(),
                        peer: peer.clone(),
                        bytes: set_message_interval_frame(
                            gcs,
                            system_id,
                            component,
                            sequence,
                            message_id,
                            interval_us,
                        ),
                    });
                }
                effects
            }
            Some(_) => Vec::new(),
        }
    }

    /// The protocols' timeouts, and the first announcement of each
    /// vehicle's boot once it has its lasting id.
    fn tick_transfers(&mut self, now_us: u64) -> Vec<Effect> {
        let timing = self.config.transfers;
        let attempts = self.config.version_attempts;
        let keys: Vec<(String, String, u8)> = self.vehicles.keys().cloned().collect();
        let mut effects = Vec::new();
        for key in keys {
            let Some(vehicle) = self.vehicles.get_mut(&key) else {
                continue;
            };
            let vehicle_id = vehicle.observation.id.clone();
            let identified =
                vehicle.observation.uid.is_some() || vehicle.version_requests >= attempts;
            if !vehicle.boot_announced
                && identified
                && let Some(boot_instant_us) = vehicle.boot_instant_us
            {
                vehicle.boot_announced = true;
                effects.push(Effect::Booted {
                    vehicle_id: vehicle_id.clone(),
                    boot_instant_us,
                    restarted: false,
                });
            }
            let mut outs = Vec::new();
            let ftp = vehicle.ftp.tick(now_us, &timing);
            let batched = !vehicle.batch.is_empty();
            if let Some(download) = vehicle.params.as_mut() {
                match download.protocol.tick(now_us, &timing) {
                    Step::Send(more) => {
                        if !more.is_empty() {
                            let (received, total) = download.protocol.progress();
                            effects.push(Effect::Progress {
                                vehicle_id: vehicle_id.clone(),
                                transfer: TransferKind::Parameters,
                                done: as_u32(received),
                                total: total.map(u32::from),
                                attempt: download.protocol.attempt(),
                            });
                        }
                        outs.extend(more);
                    }
                    Step::Done(_) => {}
                    Step::Failed(failure) => {
                        vehicle.params = None;
                        effects.push(Effect::Parameters {
                            vehicle_id: vehicle_id.clone(),
                            result: Err(failure),
                            defaults: BTreeMap::new(),
                        });
                    }
                }
            }
            let mut index = 0;
            while index < vehicle.writes.len() {
                match vehicle.writes[index].protocol.tick(now_us, &timing) {
                    Step::Send(more) => {
                        outs.extend(more);
                        index += 1;
                    }
                    Step::Done(()) | Step::Failed(_) => {
                        let write = vehicle.writes.remove(index);
                        effects.push(Effect::ParamWritten {
                            order_id: write.protocol.order_id.clone(),
                            result: Err(Failure::Timeout),
                            attempts: write.protocol.attempts(),
                        });
                    }
                }
            }
            effects.extend(self.outgoing(&key, outs));
            effects.extend(self.ftp_output(&key, ftp, now_us));
            if batched {
                effects.extend(self.flush_batch(&key, now_us));
            }
            let step = self
                .downloading(&key)
                .map(|d| d.protocol.tick(now_us, &timing));
            effects.extend(self.download_step(&key, step));
            let step = self
                .uploading(&key)
                .map(|u| u.protocol.tick(now_us, &timing));
            effects.extend(self.upload_step(&key, step));
        }
        effects
    }

    /// Normalized pilot inputs to the vehicle's current address. ArduPilot
    /// aircraft, Rover and Blimp use calibrated RC overrides; PX4 and Sub
    /// retain their firmware's `MANUAL_CONTROL` semantics.
    pub fn manual_control(&mut self, vehicle_id: &str, sticks: [i16; 4]) -> Vec<Effect> {
        self.pilot_control(vehicle_id, sticks, &[])
    }

    pub fn pilot_control(
        &mut self,
        vehicle_id: &str,
        sticks: [i16; 4],
        auxiliary: &[(u8, i16)],
    ) -> Vec<Effect> {
        self.pilot_control_with_method(
            vehicle_id,
            sticks,
            auxiliary,
            uxduck_core::domain::settings::PilotInputMethod::Automatic,
        )
    }

    pub fn pilot_control_with_method(
        &mut self,
        vehicle_id: &str,
        sticks: [i16; 4],
        auxiliary: &[(u8, i16)],
        method: uxduck_core::domain::settings::PilotInputMethod,
    ) -> Vec<Effect> {
        let Some(key) = self.find(vehicle_id) else {
            return Vec::new();
        };
        let vehicle = &self.vehicles[&key];
        let family = crate::modes::Family::for_vehicle(
            &vehicle.observation.autopilot,
            &vehicle.observation.vehicle_type,
        );
        if method == uxduck_core::domain::settings::PilotInputMethod::ManualControl
            && family == Some(crate::modes::Family::Blimp)
        {
            return self.release_control(vehicle_id);
        }
        let channels = match (method.effective(family), family) {
            (uxduck_core::domain::settings::PilotInputMethod::RcChannelsOverride, Some(family)) => {
                if !vehicle.rc_input.ready() {
                    // Persisted engine parameters can be current after a daemon
                    // restart while this new wire session has no calibration.
                    if !vehicle.ftp.reading_parameters() && vehicle.params.is_none() {
                        let now_us = vehicle.observation.last_heartbeat_us;
                        return self.download_parameters(vehicle_id, now_us);
                    }
                    return Vec::new();
                }
                let Some(channels) = vehicle.rc_input.channels(family, sticks, auxiliary) else {
                    return self.release_control(vehicle_id);
                };
                Some(channels)
            }
            (uxduck_core::domain::settings::PilotInputMethod::RcChannelsOverride, None) => {
                return self.release_control(vehicle_id);
            }
            _ if !auxiliary.is_empty() => return Vec::new(),
            _ => None,
        };
        let component = vehicle.autopilot_component;
        if let Some(channels) = channels.filter(|_| family != Some(crate::modes::Family::Px4)) {
            let Some(vehicle) = self.vehicles.get_mut(&key) else {
                return Vec::new();
            };
            for (active, value) in vehicle.rc_overridden.iter_mut().zip(channels) {
                *active |= value != u16::MAX;
            }
        }
        let sequence = self.next_sequence();
        let (link_id, peer, system_id) = key;
        let bytes = if let Some(channels) = channels {
            crate::sim::rc_override_frame(
                self.config.gcs_system_id,
                system_id,
                component,
                sequence,
                channels,
            )
        } else {
            crate::sim::manual_control_frame(self.config.gcs_system_id, system_id, sequence, sticks)
        };
        vec![Effect::Send {
            link_id,
            peer,
            bytes,
        }]
    }

    /// Return only channels UXDuck has overridden to the receiver on release,
    /// unplug or target change. Extension channels use a different sentinel.
    pub fn release_control(&mut self, vehicle_id: &str) -> Vec<Effect> {
        let Some(key) = self.find(vehicle_id) else {
            return Vec::new();
        };
        let Some(vehicle) = self.vehicles.get_mut(&key) else {
            return Vec::new();
        };
        if !vehicle.rc_overridden.iter().any(|active| *active) {
            return Vec::new();
        }
        let mut channels = [u16::MAX; 18];
        for (index, active) in vehicle.rc_overridden.iter_mut().enumerate() {
            if *active {
                channels[index] = if index < 8 { 0 } else { u16::MAX - 1 };
            }
            *active = false;
        }
        let component = vehicle.autopilot_component;
        let sequence = self.next_sequence();
        let (link_id, peer, system_id) = key;
        vec![Effect::Send {
            link_id,
            peer,
            bytes: crate::sim::rc_override_frame(
                self.config.gcs_system_id,
                system_id,
                component,
                sequence,
                channels,
            ),
        }]
    }

    /// A frame for a vehicle's current address, made with the next sequence
    /// number: a plugin's message by name (S54). `make` gets the vehicle's
    /// system id and the sequence. Nothing if it has no address.
    pub fn send_to(
        &mut self,
        vehicle_id: &str,
        make: impl FnOnce(u8, u8) -> Vec<u8>,
    ) -> Vec<Effect> {
        let Some((link_id, peer, system_id)) = self.find(vehicle_id) else {
            return Vec::new();
        };
        let sequence = self.next_sequence();
        vec![Effect::Send {
            link_id,
            peer,
            bytes: make(system_id, sequence),
        }]
    }

    /// The VID of the vehicle at this address, if one is known.
    #[must_use]
    pub fn vehicle_at(&self, link_id: &str, peer: &str, system_id: u8) -> Option<&str> {
        self.vehicles
            .get(&(link_id.to_owned(), peer.to_owned(), system_id))
            .map(|v| v.observation.id.as_str())
    }

    /// The system id and VID of each vehicle at this link and peer.
    pub fn vehicles_at_peer<'a>(
        &'a self,
        link_id: &str,
        peer: &str,
    ) -> impl Iterator<Item = (u8, &'a str)> + 'a {
        let (link_id, peer) = (link_id.to_owned(), peer.to_owned());
        self.vehicles
            .range((link_id.clone(), peer.clone(), 0)..=(link_id, peer, u8::MAX))
            .map(|((_, _, system_id), v)| (*system_id, v.observation.id.as_str()))
    }

    /// Where each vehicle is reached now, for traffic to it.
    #[must_use]
    pub fn routes(&self) -> Vec<crate::router::VehicleRoute> {
        self.vehicles
            .values()
            .map(|v| crate::router::VehicleRoute {
                vehicle_id: v.observation.id.clone(),
                link_id: v.observation.link_id.clone(),
                peer: v.observation.peer.clone(),
            })
            .collect()
    }

    /// Whether `tick` has work: pending telemetry, a vehicle not yet offline,
    /// or a command or transfer in flight. With none, the data plane stops its
    /// timer, so an idle daemon does not wake (S24). Peers expire at the next tick
    /// whenever that is, which only matters once frames arrive again.
    #[must_use]
    pub fn needs_tick(&self) -> bool {
        !self.commands.is_empty()
            || !self.terrain.is_empty()
            || !self.terrain_checks.is_empty()
            || self.vehicles.values().any(|v| {
                v.observation.presence != Presence::Offline
                    || !v.pending.items.is_empty()
                    || !v.pending.positions.is_empty()
                    || v.params.is_some()
                    || v.ftp.busy()
                    || !v.batch.is_empty()
                    || !v.writes.is_empty()
                    || v.mission_download.is_some()
                    || v.mission_upload.is_some()
            })
    }

    /// Whether any vehicle is online (the wake lock).
    #[must_use]
    pub fn any_online(&self) -> bool {
        self.vehicles
            .values()
            .any(|v| v.observation.presence == Presence::Online)
    }

    /// Forgets every vehicle on a link, as removing the link does.
    pub fn remove_link(&mut self, link_id: &str) -> Vec<String> {
        let gone: Vec<(String, String, u8)> = self
            .vehicles
            .keys()
            .filter(|(link, _, _)| link == link_id)
            .cloned()
            .collect();
        let mut ids = Vec::new();
        for key in gone {
            if let Some(vehicle) = self.vehicles.remove(&key) {
                ids.push(vehicle.observation.id);
            }
        }
        self.peers.retain(|(link, _), _| link != link_id);
        self.stations.retain(|(link, _, _), _| link != link_id);
        self.simulated.remove(link_id);
        self.paused.remove(link_id);
        ids
    }
}

/// What a heartbeat says about the vehicle.
struct Beat {
    mav_type: String,
    autopilot: String,
    armed: bool,
    custom_mode: u32,
}

/// Presence has the same age thresholds at ingestion and on a timer tick.
fn presence_at(config: Config, heartbeat_us: u64, now_us: u64) -> Presence {
    let age = now_us.saturating_sub(heartbeat_us);
    if age <= config.stale_after_us {
        Presence::Online
    } else if age <= config.offline_after_us {
        Presence::Stale
    } else {
        Presence::Offline
    }
}

/// After the vehicle has been online for the settle window, request only
/// rows missing for at least five seconds and three of their intervals,
/// with backoff, and never past the last attempt.
fn missing_streams(vehicle: &mut Vehicle, config: &Config, now_us: u64) -> Vec<(u32, u32)> {
    if now_us.saturating_sub(vehicle.online_since_us) < config.stream_settle_us {
        return Vec::new();
    }
    let mut missing = Vec::new();
    for (message_id, interval_us) in STREAM_RATES {
        if SENT_WHILE_SET.contains(&message_id) {
            continue;
        }
        let Some(stream) = vehicle.streams.get_mut(&message_id) else {
            continue;
        };
        let since = stream.last_seen_us.map_or(vehicle.online_since_us, |seen| {
            seen.max(vehicle.online_since_us)
        });
        let timeout = STREAM_SILENCE_US.max(3 * u64::from(interval_us));
        let late = now_us.saturating_sub(since) > timeout;
        if stream.unavailable || !late || now_us < stream.next_try_us {
            continue;
        }
        let Some(wait) = STREAM_BACKOFF_US.get(usize::from(stream.attempts)) else {
            stream.unavailable = true;
            continue;
        };
        stream.attempts += 1;
        stream.next_try_us = now_us + wait;
        missing.push((message_id, interval_us));
    }
    missing
}

/// Applies an `AUTOPILOT_VERSION`: the UID gives the vehicle its lasting id,
/// within its simulation for a simulated one, and the provisional id
/// becomes an alias. True if anything changed.
fn identify(
    vehicle: &mut Vehicle,
    uid: u64,
    uid2: [u8; 18],
    version: (u32, [u8; 8]),
    simulation: Option<&str>,
) -> bool {
    let (flight_sw_version, flight_custom_version) = version;
    let firmware = Some(firmware_version(flight_sw_version));
    let commit = firmware_commit(flight_custom_version);
    let mut changed =
        vehicle.observation.firmware != firmware || vehicle.observation.firmware_commit != commit;
    vehicle.observation.firmware = firmware;
    vehicle.observation.firmware_commit = commit;
    if vehicle.observation.uid.is_none()
        && let (Some(text), Some(id)) = (
            uid_text(uid, uid2),
            simulation.map_or_else(
                || vid_from_uid(uid, uid2),
                |link| simulated_vid(link, uid, uid2),
            ),
        )
    {
        vehicle.observation.uid = Some(text);
        if id != vehicle.observation.id {
            let old = std::mem::replace(&mut vehicle.observation.id, id);
            vehicle.observation.aliases.push(old);
        }
        changed = true;
    }
    changed
}

#[cfg(test)]
#[allow(clippy::float_cmp, reason = "exact small integers sent as floats")]
mod tests {
    use super::*;
    use crate::sim::{
        FakeVehicle, gcs_heartbeat_frame, heartbeat_frame, status_text_frame, terrain_request_frame,
    };
    use uxduck_core::domain::command::Command;

    fn vehicles(effects: &[Effect]) -> Vec<&Observation> {
        effects
            .iter()
            .filter_map(|e| {
                if let Effect::Vehicle(v) = e {
                    Some(&**v)
                } else {
                    None
                }
            })
            .collect()
    }

    fn sends(effects: &[Effect]) -> Vec<&[u8]> {
        effects
            .iter()
            .filter_map(|e| {
                if let Effect::Send { bytes, .. } = e {
                    Some(bytes.as_slice())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Sends that are `MAV_CMD` commands, as (command, param1).
    fn commands(effects: &[Effect]) -> Vec<(u16, f32)> {
        sends(effects)
            .into_iter()
            .filter_map(|bytes| match decode(bytes)?.message {
                Message::CommandLong {
                    command, param1, ..
                } => Some((command, param1)),
                _ => None,
            })
            .collect()
    }

    fn version_requests(effects: &[Effect]) -> usize {
        commands(effects)
            .iter()
            .filter(|(command, param1)| *command == 512 && (*param1 - 148.0).abs() < 0.5)
            .count()
    }

    const PEER: &str = "127.0.0.1:5760";

    #[test]
    fn a_heartbeat_makes_a_vehicle_that_asks_for_its_uid_and_takes_the_derived_id() {
        let mut session = Session::new(Config::default());
        let mut fake = FakeVehicle::new(1, 0xABCD);
        let effects = session.ingest("l1", PEER, &fake.heartbeat(), 1_000);
        let first = vehicles(&effects);
        assert_eq!(first.len(), 1);
        assert_eq!(
            (
                first[0].system_id,
                first[0].presence,
                first[0].uid.as_deref()
            ),
            (1, Presence::Online, None)
        );
        let provisional = first[0].id.clone();

        assert_eq!(
            version_requests(&effects),
            1,
            "AUTOPILOT_VERSION is requested once"
        );
        let reply: Vec<Vec<u8>> = sends(&effects)
            .into_iter()
            .flat_map(|bytes| fake.answer(bytes))
            .collect();
        assert_eq!(reply.len(), 1, "only the version request is answered");
        let effects = session.ingest("l1", PEER, &reply[0], 2_000);
        let identified = vehicles(&effects)[0];
        assert_eq!(
            identified.id,
            vid_from_uid(0xABCD, [0; 18]).unwrap_or_default()
        );
        assert_eq!(identified.aliases, [provisional]);
        assert_eq!(identified.firmware.as_deref(), Some("4.5.1"));
        assert_eq!(identified.firmware_commit, None, "the fake sent no hash");
        assert!(
            session
                .ingest("l1", PEER, &fake.heartbeat(), 3_000)
                .is_empty(),
            "nothing new to say"
        );
    }

    #[test]
    fn a_vehicle_lists_its_own_modes_and_a_scripted_one_goes_by_its_name() {
        let mut session = Session::new(Config::default());
        let mut fake = FakeVehicle::new(1, 0xABCD);
        fake.modes = vec![
            AvailableMode::new(5, "LOITER", 0),
            AvailableMode::new(100, "Survey", 0),
        ];
        let effects = session.ingest("l1", PEER, &fake.heartbeat(), 1_000);
        let reply: Vec<Vec<u8>> = sends(&effects)
            .into_iter()
            .flat_map(|bytes| fake.answer(bytes))
            .collect();
        assert_eq!(reply.len(), 3, "the version, then one frame per mode");
        let effects: Vec<Effect> = reply
            .iter()
            .flat_map(|bytes| session.ingest("l1", PEER, bytes, 2_000))
            .collect();
        let listed = effects
            .iter()
            .find_map(|e| match e {
                Effect::Modes { modes, .. } => Some(modes.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no list in {effects:?}"));
        assert_eq!(
            listed.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            ["LOITER", "Survey"]
        );
        // The list again says nothing new.
        let again: Vec<Vec<u8>> = fake.answer(&crate::sim::request_message_frame(
            crate::GCS_SYSTEM_ID,
            1,
            1,
            9,
            435,
            0.0,
        ));
        assert!(
            again
                .iter()
                .flat_map(|bytes| session.ingest("l1", PEER, bytes, 3_000))
                .all(|e| !matches!(e, Effect::Modes { .. }))
        );
        // The scripted mode is named from the list, and set by that name.
        fake.mode = 100;
        let effects = session.ingest("l1", PEER, &fake.heartbeat(), 4_000);
        assert_eq!(vehicles(&effects)[0].mode.as_deref(), Some("Survey"));
        let id = vehicles(&effects)[0].id.clone();
        fake.mode = 0;
        let order = CommandOrder {
            receipt_id: "r1".into(),
            vehicle_id: id,
            command: Command::SetMode {
                mode: "survey".into(),
            },
            force: false,
        };
        for bytes in sends(&session.command(order, 5_000)) {
            fake.answer(bytes);
        }
        assert_eq!(fake.mode, 100);
        // A changed list is asked for again, but not the first count heard.
        let first = session.ingest(
            "l1",
            PEER,
            &crate::sim::available_modes_monitor_frame(1, 1, 20, 1),
            6_000,
        );
        assert!(sends(&first).is_empty());
        let changed = session.ingest(
            "l1",
            PEER,
            &crate::sim::available_modes_monitor_frame(1, 1, 21, 2),
            7_000,
        );
        assert_eq!(sends(&changed).len(), 1, "asked again");
    }

    #[test]
    fn a_vehicles_messages_come_whole_and_a_long_one_in_one_piece() {
        let mut session = Session::new(Config::default());
        let mut fake = FakeVehicle::new(1, 0xABCD);
        fake.startup_burst = true;
        session.ingest("l1", PEER, &fake.heartbeat(), 1_000);
        let mut said = Vec::new();
        for frame in fake.status_texts(2.0) {
            for effect in session.ingest("l1", PEER, &frame, 2_000) {
                if let Effect::StatusText { severity, text, .. } = effect {
                    said.push((severity, text));
                }
            }
        }
        assert_eq!(said.len(), 9, "eight whole and one in two parts: {said:?}");
        assert!(said.contains(&(4, "Compass not calibrated".into())));
        assert_eq!(
            said.last().unwrap().1,
            "EKF3 IMU0 initialised; waiting for a good GPS position before the origin is set"
        );
        // Nothing from a vehicle the session has not heard.
        let stranger = status_text_frame(9, 0, 4, "Who?", (0, 0));
        assert!(
            session
                .ingest("l1", PEER, &stranger, 3_000)
                .iter()
                .all(|e| !matches!(e, Effect::StatusText { .. }))
        );
    }

    #[test]
    fn the_version_brings_the_commit_it_was_built_from() {
        let mut session = Session::new(Config::default());
        let mut fake = FakeVehicle::new(1, 0xABCD);
        fake.flight_sw_version = 0x0405_0800;
        fake.commit = *b"2A3DC4B7";
        let effects = session.ingest("l1", PEER, &fake.heartbeat(), 1_000);
        let reply: Vec<Vec<u8>> = sends(&effects)
            .into_iter()
            .flat_map(|bytes| fake.answer(bytes))
            .collect();
        let effects = session.ingest("l1", PEER, &reply[0], 2_000);
        let identified = vehicles(&effects)[0];
        assert_eq!(identified.firmware.as_deref(), Some("4.5.8"));
        assert_eq!(identified.firmware_commit.as_deref(), Some("2a3dc4b7"));
    }

    #[test]
    fn the_tick_is_needed_only_while_a_vehicle_is_heard() {
        let mut session = Session::new(Config {
            stale_after_us: 100,
            offline_after_us: 200,
            ..Config::default()
        });
        assert!(!session.needs_tick(), "no vehicles: nothing to tick");
        let mut fake = FakeVehicle::new(1, 0xABCD);
        session.ingest("l1", PEER, &fake.heartbeat(), 1_000);
        assert!(session.needs_tick(), "an online vehicle is ticked");
        session.tick(1_150);
        assert!(session.needs_tick(), "and a stale one");
        session.tick(1_300);
        assert!(!session.needs_tick(), "an offline one is not");
        session.ingest("l1", PEER, &fake.heartbeat(), 2_000);
        assert!(session.needs_tick(), "until it is heard again");
    }

    #[test]
    fn presence_goes_stale_then_offline_and_back() {
        let mut session = Session::new(Config {
            stale_after_us: 100,
            offline_after_us: 1_000,
            ..Config::default()
        });
        session.ingest("l1", PEER, &heartbeat_frame(1, 1, 0), 0);
        assert!(vehicles(&session.tick(50)).is_empty());
        assert_eq!(vehicles(&session.tick(200))[0].presence, Presence::Stale);
        assert_eq!(
            vehicles(&session.tick(2_000))[0].presence,
            Presence::Offline
        );
        let back = session.ingest("l1", PEER, &heartbeat_frame(1, 1, 1), 2_100);
        assert_eq!(vehicles(&back)[0].presence, Presence::Online);
    }

    /// A simulated vehicle's system id changed by a parameter write (a
    /// snapshot's `MAV_SYSID`): the same autopilot at a new address on the
    /// same peer. The old address is let go once the UID says so, so it
    /// does not report the vehicle stale and then offline while it flies.
    #[test]
    fn a_vehicle_whose_system_id_changes_moves_to_its_new_address() {
        let mut fake = FakeVehicle::new(2, 0xABCD);
        let (mut session, vid) = identified(&mut fake);
        assert_eq!(session.vehicle_at("l1", PEER, 2), Some(vid.as_str()));

        fake.system_id = 1;
        let heard = session.ingest("l1", PEER, &fake.heartbeat(), 500_000);
        let rest = converse(&mut session, &mut fake, heard, 500_000);
        let moved = vehicles(&rest);
        let last = moved.last().expect("identified at its new address");
        assert_eq!((last.id.as_str(), last.system_id), (vid.as_str(), 1));
        assert_eq!(session.vehicle_at("l1", PEER, 1), Some(vid.as_str()));
        assert_eq!(
            session.vehicle_at("l1", PEER, 2),
            None,
            "the old address is gone"
        );

        // Heartbeats keep coming at the new address only: nothing goes stale.
        for second in 1..30_u64 {
            let now = second * 1_000_000;
            session.ingest("l1", PEER, &fake.heartbeat(), now);
            let changes = session.tick(now);
            assert!(
                vehicles(&changes)
                    .iter()
                    .all(|v| v.presence == Presence::Online),
                "{changes:?}"
            );
        }
    }

    #[test]
    fn a_paused_simulator_keeps_its_vehicle_until_it_resumes() {
        let mut session = Session::new(Config {
            stale_after_us: 100,
            offline_after_us: 1_000,
            ..Config::default()
        });
        session.ingest("l1", PEER, &heartbeat_frame(1, 1, 0), 0);
        session.pause("l1", true, 50);
        assert!(session.is_paused("l1"));
        // Silent for far longer than the vehicle would be offline, and
        // nothing asked of it.
        let quiet = session.tick(60_000);
        assert!(
            vehicles(&quiet).is_empty(),
            "no presence change while paused"
        );
        assert!(
            !quiet.iter().any(|e| matches!(e, Effect::Send { .. })),
            "nothing sent to a paused simulator"
        );
        session.pause("l1", false, 60_000);
        assert!(!session.is_paused("l1"));
        assert!(
            vehicles(&session.tick(60_050)).is_empty(),
            "online on resuming"
        );
        assert_eq!(
            vehicles(&session.tick(60_200))[0].presence,
            Presence::Stale,
            "silence after resuming counts again"
        );
    }

    #[test]
    fn queued_heartbeats_after_sleep_do_not_flash_online() {
        let mut session = Session::new(Config::default());
        session.ingest("l1", PEER, &heartbeat_frame(1, 1, 0), 1_000_000);
        let wake = 421_000_000;
        assert_eq!(vehicles(&session.tick(wake))[0].presence, Presence::Offline);
        // The captured failure: old heartbeats go through while the tick
        // uses wall time, repeatedly turning the vehicle online/offline.
        for sequence in 1..5 {
            let effects = session.ingest_received(
                "l1",
                PEER,
                &heartbeat_frame(1, 1, sequence),
                1_000_000 + u64::from(sequence) * 1_000_000,
                wake,
            );
            assert!(
                vehicles(&effects)
                    .iter()
                    .all(|v| v.presence == Presence::Offline)
            );
            assert!(vehicles(&session.tick(wake + 100_000)).is_empty());
        }
        let back = session.ingest_received("l1", PEER, &heartbeat_frame(1, 1, 5), wake, wake);
        assert_eq!(vehicles(&back)[0].presence, Presence::Online);
    }

    #[test]
    fn a_first_queued_heartbeat_is_discovered_with_its_actual_age() {
        for (delay, expected) in [
            (3_000_000, Presence::Online),
            (3_000_001, Presence::Stale),
            (15_000_000, Presence::Stale),
            (15_000_001, Presence::Offline),
        ] {
            let mut session = Session::new(Config::default());
            let effects =
                session.ingest_received("l1", PEER, &heartbeat_frame(1, 1, 0), 1, 1 + delay);
            assert_eq!(vehicles(&effects)[0].presence, expected);
            assert_eq!(vehicles(&effects)[0].last_heartbeat_us, 1);
            assert!(vehicles(&session.tick(1 + delay)).is_empty());
        }
    }

    #[test]
    fn queued_telemetry_flushes_even_when_every_vehicle_is_offline() {
        let mut session = Session::new(Config::default());
        let mut fake = FakeVehicle::new(1, 0xABCD);
        session.ingest("l1", PEER, &fake.heartbeat(), 1_000_000);
        let wake = 421_000_000;
        session.tick(wake);
        assert!(!session.needs_tick());
        session.ingest_received("l1", PEER, &fake.telemetry(0.0).concat(), 2_000_000, wake);
        assert!(
            session.needs_tick(),
            "old telemetry cannot accumulate without a flush"
        );
        let effects = session.tick(wake);
        let update = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::Telemetry(update) => Some(update),
                _ => None,
            })
            .expect("pending telemetry is flushed");
        assert!(update.items.iter().all(|item| item.time_us == 2_000_000));
        assert!(
            update
                .positions
                .iter()
                .all(|position| position.time_us == 2_000_000)
        );
        assert!(
            !session.needs_tick(),
            "the timer stops after the pending data is flushed"
        );
    }

    #[test]
    fn the_same_system_id_on_two_peers_is_two_vehicles() {
        let mut session = Session::new(Config::default());
        session.ingest("l1", "10.0.0.1:14550", &heartbeat_frame(1, 1, 0), 0);
        let effects = session.ingest("l1", "10.0.0.2:14550", &heartbeat_frame(1, 1, 0), 0);
        let second = vehicles(&effects)[0];
        assert_eq!(second.peer, "10.0.0.2:14550");
        assert_eq!(session.vehicles.len(), 2);
    }

    #[test]
    fn ground_stations_are_not_vehicles_and_terrain_requests_go_to_the_engine() {
        let mut session = Session::new(Config::default());
        assert!(
            session
                .ingest("l1", PEER, &gcs_heartbeat_frame(255, 190, 0), 0)
                .is_empty()
        );
        session.ingest("l1", PEER, &heartbeat_frame(1, 1, 0), 0);
        let block = crate::terrain::Block {
            lat: 530_646_790,
            lon: -40_798_165,
            spacing: 100,
        };
        let frame = crate::sim::terrain_block_request_frame(1, 1, 2, block, 0b1011);
        let effects = session.ingest("l1", PEER, &frame, 10);
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::TerrainRequest { block: b, mask: 0b1011, .. }] if *b == block
            ),
            "{effects:?}"
        );
        // An empty request from a vehicle is still a request.
        let empty = session.ingest("l1", PEER, &terrain_request_frame(1, 1, 3), 20);
        assert!(matches!(
            empty.as_slice(),
            [Effect::TerrainRequest { mask: 0, .. }]
        ));
    }

    #[test]
    fn a_checks_report_is_matched_to_it_and_is_not_the_ground_under_the_vehicle() {
        let mut session = Session::new(Config::default());
        let mut fake = FakeVehicle::new(1, 0xABCD);
        session.ingest("l1", PEER, &fake.heartbeat(), 0);
        let vehicle_id = session.vehicle_at("l1", PEER, 1).unwrap().to_owned();
        let (lat, lon) = (-353_000_000, 1_491_000_000);
        let effects = session.terrain_check(&vehicle_id, lat, lon, 10);
        let [Effect::Send { bytes, .. }] = effects.as_slice() else {
            panic!("{effects:?}");
        };
        // The fake vehicle answers with its fixed terrain, 580 m.
        let mut checked = Vec::new();
        for reply in fake.answer(bytes) {
            checked.extend(session.ingest("l1", PEER, &reply, 20));
        }
        assert!(
            matches!(
                checked.as_slice(),
                [Effect::TerrainChecked { lat: a, lon: b, height: Some(Some(h)), .. }]
                    if *a == lat && *b == lon && (*h - 580.0).abs() < 0.1
            ),
            "{checked:?}"
        );
        // Nothing went to the ground under it.
        assert!(
            session
                .tick(30)
                .iter()
                .all(|e| !matches!(e, Effect::Telemetry(_)))
        );
        // A check that gets no answer is reported as such after five seconds.
        session.terrain_check(&vehicle_id, lat + 1, lon, 100);
        let late = session.tick(6_000_000);
        assert!(
            late.iter()
                .any(|e| matches!(e, Effect::TerrainChecked { height: None, .. }))
        );
    }

    #[test]
    fn terrain_answers_go_to_the_vehicle_at_their_pace() {
        let mut session = Session::new(Config::default());
        session.ingest("l1", PEER, &heartbeat_frame(1, 1, 0), 0);
        let vehicle_id = session.vehicle_at("l1", PEER, 1).unwrap().to_owned();
        let block = crate::terrain::Block {
            lat: 530_646_790,
            lon: -40_798_165,
            spacing: 100,
        };
        let groups = (0..6)
            .map(|gridbit| crate::terrain::Group {
                gridbit,
                heights: [584; 16],
            })
            .collect();
        let sent = |effects: Vec<Effect>| {
            effects
                .into_iter()
                .filter(|e| {
                    matches!(e, Effect::Send { bytes, .. }
                    if crate::framing::header(bytes).is_some_and(|h| h.message_id == 134))
                })
                .count()
        };
        // One at once, then four a second, as a serial radio's pace.
        assert_eq!(
            sent(session.terrain_data(&vehicle_id, block, groups, u64::MAX, 4, 100)),
            1
        );
        assert!(session.needs_tick());
        assert_eq!(sent(session.tick(600_100)), 2);
        assert_eq!(sent(session.tick(1_100_100)), 2);
        assert_eq!(sent(session.tick(5_000_000)), 1);
    }

    #[test]
    fn a_change_is_another_ground_stations_only_while_one_is_heard() {
        const S: u64 = 1_000_000;
        let station = |effects: &[Effect]| {
            effects.iter().find_map(|e| match e {
                Effect::ParamValue { station, .. } => Some(*station),
                _ => None,
            })
        };
        let changed = |session: &mut Session, at: u64| {
            let frame = crate::sim::param_value_frame(1, 1, 0, "ARSPD_USE", 1.0, 9, 900, u16::MAX);
            station(&session.ingest("l1", PEER, &frame, at))
        };
        let mut session = Session::new(Config::default());
        session.ingest("l1", PEER, &heartbeat_frame(1, 1, 0), 0);
        assert_eq!(
            changed(&mut session, S),
            Some(false),
            "nobody else: the vehicle"
        );

        session.ingest(
            "l1",
            "10.0.0.9:14550",
            &gcs_heartbeat_frame(255, 190, 0),
            2 * S,
        );
        assert_eq!(changed(&mut session, 3 * S), Some(true));
        assert_eq!(changed(&mut session, 13 * S), Some(false), "gone quiet");

        // One behind a forward, whose frames the session does not read.
        session.forwarded(
            "fwd",
            "127.0.0.1:14550",
            &gcs_heartbeat_frame(255, 190, 1),
            20 * S,
        );
        assert_eq!(changed(&mut session, 21 * S), Some(true));
        session.remove_link("fwd");
        assert_eq!(changed(&mut session, 22 * S), Some(false));
    }

    #[test]
    fn a_simulated_vehicle_is_new_with_each_simulation() {
        let identified = |session: &mut Session, link: &str| {
            let mut fake = FakeVehicle::new(1, 0xABCD);
            let effects = session.ingest(link, PEER, &fake.heartbeat(), 1_000);
            let reply: Vec<Vec<u8>> = sends(&effects)
                .into_iter()
                .flat_map(|bytes| fake.answer(bytes))
                .collect();
            vehicles(&session.ingest(link, PEER, &reply[0], 2_000))[0].clone()
        };
        let mut session = Session::new(Config::default());
        session.simulated("sim-a");
        session.simulated("sim-b");
        let a = identified(&mut session, "sim-a");
        let b = identified(&mut session, "sim-b");
        let real = identified(&mut session, "serial");
        assert_eq!(
            a.id,
            simulated_vid("sim-a", 0xABCD, [0; 18]).unwrap_or_default()
        );
        assert_ne!(a.id, b.id, "every simulator of a build has one UID");
        assert_eq!(a.uid, b.uid);
        assert_eq!(real.id, vid_from_uid(0xABCD, [0; 18]).unwrap_or_default());
    }

    #[test]
    fn the_version_request_is_retried_a_few_times() {
        let mut session = Session::new(Config {
            version_retry_us: 10,
            version_attempts: 3,
            ..Config::default()
        });
        let mut sent = version_requests(&session.ingest("l1", PEER, &heartbeat_frame(1, 1, 0), 0));
        for t in 1..10u64 {
            session.ingest("l1", PEER, &heartbeat_frame(1, 1, 0), t * 10);
            sent += version_requests(&session.tick(t * 10));
        }
        assert_eq!(sent, 3);
    }

    #[test]
    fn brief_stream_gaps_do_not_reconfigure_rates() {
        let s = 1_000_000;
        let mut session = Session::new(Config::default());
        let mut fake = FakeVehicle::new(1, 0xABCD);
        let first = session.ingest("l1", PEER, &fake.heartbeat(), 0);
        converse(&mut session, &mut fake, first, 0);

        // A fast stream arrives in batches once a second for twenty seconds.
        // These brief delays must not repeatedly reset its configured rate.
        for second in 0..20u64 {
            let now = second * s;
            session.ingest("l1", PEER, &fake.heartbeat(), now);
            #[allow(clippy::cast_precision_loss, reason = "test times are small")]
            let frames = fake.telemetry(second as f64).concat();
            session.ingest("l1", PEER, &frames, now);
            let effects = session.tick(now + 900_000);
            assert!(
                commands(&effects)
                    .iter()
                    .all(|(command, _)| *command != 511),
                "a 900 ms delay must not reconfigure streams: {effects:?}"
            );
        }
    }

    #[test]
    fn a_copter_is_asked_for_where_it_is_going_and_where_home_is() {
        // ArduCopter streams nothing it is not asked for, so without these
        // the map has no line to a GUIDED target and none home in RTL, and
        // there is no wind or rangefinder.
        let s = 1_000_000;
        let mut session = Session::new(Config::default());
        let mut fake = FakeVehicle::new(1, 0xABCD);
        let effects = session.ingest("l1", PEER, &fake.heartbeat(), s);
        let asked: Vec<f32> = commands(&effects)
            .into_iter()
            .filter(|(command, _)| *command == 511)
            .map(|(_, message_id)| message_id)
            .collect();
        for (message_id, name) in [
            (87.0, "POSITION_TARGET_GLOBAL_INT"),
            (242.0, "HOME_POSITION"),
            (168.0, "WIND"),
            (132.0, "DISTANCE_SENSOR"),
        ] {
            assert!(
                asked.contains(&message_id),
                "{name} is not asked for: {asked:?}"
            );
        }
    }

    #[test]
    fn stream_rates_are_reconciled_only_while_heartbeats_arrive_with_backoff() {
        let s = 1_000_000;
        let mut session = Session::new(Config::default());
        let rates = |effects: &[Effect]| {
            commands(effects)
                .into_iter()
                .filter(|(command, _)| *command == 511)
                .map(|(_, message_id)| message_id)
                .collect::<Vec<f32>>()
        };
        let mut fake = FakeVehicle::new(1, 0xABCD);
        let first = session.ingest("l1", PEER, &fake.heartbeat(), 0);
        assert_eq!(
            rates(&first),
            [
                33.0, 30.0, 1.0, 24.0, 74.0, 36.0, 42.0, 62.0, 193.0, 241.0, 147.0, 136.0, 87.0,
                242.0, 168.0, 132.0, 133.0, 162.0, 285.0
            ],
            "the whole table once"
        );
        converse(&mut session, &mut fake, first, 0);

        // Heartbeats on time, and every stream but ATTITUDE (30) arriving.
        let mut asked = Vec::new();
        for tick in 1..=100u64 {
            let now = tick * s / 2;
            session.ingest("l1", PEER, &fake.heartbeat(), now);
            #[allow(clippy::cast_precision_loss, reason = "test times are small")]
            let frames: Vec<Vec<u8>> = fake
                .telemetry(now as f64 / 1e6)
                .into_iter()
                .filter(|f| crate::framing::header(f).is_some_and(|h| h.message_id != 30))
                .collect();
            session.ingest("l1", PEER, &frames.concat(), now);
            for id in rates(&session.tick(now)) {
                asked.push((now / s, id));
            }
        }
        // After five seconds of silence, then 5 and 15 s later; the 60 s wait
        // runs past this test; nothing else is asked for again.
        let times: Vec<u64> = asked.iter().map(|(t, _)| *t).collect();
        assert!(asked.iter().all(|(_, id)| *id == 30.0), "{asked:?}");
        assert_eq!(times.len(), 3, "{asked:?}");
        assert_eq!(times[0], 5, "wait past five seconds before recovering");
        assert_eq!(times[1] - times[0], 5);
        assert_eq!(times[2] - times[1], 15);

        // While the vehicle is stale nothing is asked for.
        let quiet = session.tick(60 * s);
        assert!(rates(&quiet).is_empty());
    }

    #[test]
    fn a_resumed_position_stream_can_be_recovered_after_another_interruption() {
        let s = 1_000_000;
        // Resume during the final backoff, and after the stream was marked
        // unavailable. Neither state should survive proof that it works.
        for resumed_at in [27u64, 90] {
            let mut session = Session::new(Config::default());
            let mut fake = FakeVehicle::new(1, 0xABCD);
            let first = session.ingest("l1", PEER, &fake.heartbeat(), 0);
            converse(&mut session, &mut fake, first, 0);
            let mut requests = 0;
            for second in 1..=resumed_at + 6 {
                let now = second * s;
                session.ingest("l1", PEER, &fake.heartbeat(), now);
                #[allow(clippy::cast_precision_loss, reason = "test times are small")]
                let frames: Vec<Vec<u8>> = fake
                    .telemetry(second as f64)
                    .into_iter()
                    .filter(|frame| {
                        second == resumed_at
                            || crate::framing::header(frame)
                                .is_some_and(|header| header.message_id != 33)
                    })
                    .collect();
                session.ingest("l1", PEER, &frames.concat(), now);
                let effects = session.tick(now);
                let requested = commands(&effects)
                    .iter()
                    .any(|(command, message_id)| *command == 511 && *message_id == 33.0);
                if second < resumed_at {
                    requests += usize::from(requested);
                }
                if second == resumed_at {
                    assert_eq!(requests, 3, "exhaust the first recovery attempts");
                    assert!(effects.iter().any(|effect| {
                        matches!(effect, Effect::Telemetry(update) if !update.positions.is_empty())
                    }));
                }
                if second == resumed_at + 5 {
                    assert!(!requested, "keep the five-second silence allowance");
                }
                if second == resumed_at + 6 {
                    assert!(requested, "recover again after resuming at {resumed_at}s");
                }
            }
        }
    }

    #[test]
    fn a_command_is_unicast_acknowledged_and_changes_what_the_heartbeat_says() {
        let mut session = Session::new(Config::default());
        let mut fake = FakeVehicle::new(1, 0xABCD);
        session.ingest("l1", PEER, &fake.heartbeat(), 0);
        let vid = session.vehicle_at("l1", PEER, 1).unwrap().to_owned();
        let order = |receipt: &str, command: Command| CommandOrder {
            receipt_id: receipt.into(),
            vehicle_id: vid.clone(),
            command,
            force: false,
        };
        let sent = session.command(order("r1", Command::Arm), 10);
        let [
            Effect::Send {
                link_id,
                peer,
                bytes,
            },
        ] = sent.as_slice()
        else {
            panic!("{sent:?}");
        };
        assert_eq!(
            (link_id.as_str(), peer.as_str()),
            ("l1", PEER),
            "to its own peer"
        );
        let acks = fake.answer(bytes);
        let done = session.ingest("l1", PEER, &acks[0], 20);
        assert!(matches!(
            done.as_slice(),
            [Effect::CommandDone { receipt_id, outcome: CommandOutcome::Accepted, attempts: 1 }]
                if receipt_id == "r1"
        ));
        let seen = session.ingest("l1", PEER, &fake.heartbeat(), 30);
        let observed = vehicles(&seen);
        assert_eq!(
            (observed[0].armed, observed[0].mode.as_deref()),
            (true, Some("STABILIZE"))
        );

        let sent = session.command(
            order(
                "r2",
                Command::SetMode {
                    mode: "LOITER".into(),
                },
            ),
            40,
        );
        let Effect::Send { bytes, .. } = &sent[0] else {
            panic!()
        };
        session.ingest("l1", PEER, &fake.answer(bytes)[0], 50);
        let seen = session.ingest("l1", PEER, &fake.heartbeat(), 60);
        assert_eq!(vehicles(&seen)[0].mode.as_deref(), Some("LOITER"));
    }

    fn submarine_go_to_fixture(simulated: bool) -> (Session, CommandOrder) {
        let mut session = Session::new(Config::default());
        if simulated {
            session.simulated.insert("l1".into());
        }
        session.ingest("l1", PEER, &crate::sim::heartbeat_frame(1, 1, 0), 1_000);
        let vehicle = session.vehicles.values_mut().next().unwrap();
        vehicle.observation.vehicle_type = "Submarine".into();
        vehicle.position = Some((37.763_932, -122.385_804, 1_000));
        vehicle.heading = Some((91.0, 1_000));
        let order = CommandOrder {
            receipt_id: "goto".into(),
            vehicle_id: vehicle.observation.id.clone(),
            command: Command::GoTo {
                latitude_deg: 37.765_026_5,
                longitude_deg: -122.384_205_6,
                altitude_m: -5.0,
                altitude_frame: AltitudeFrame::AboveHome,
                heading: None,
            },
            force: false,
        };
        (session, order)
    }

    fn submarine_ack(session: &mut Session, command: u16, result: u8, at: u64) -> Vec<Effect> {
        session.ingest(
            "l1",
            PEER,
            &crate::sim::command_ack_frame(1, 1, (255, 190), 0, command, result),
            at,
        )
    }

    #[test]
    fn simulated_submarine_go_to_faces_the_target_after_the_destination_is_accepted() {
        let (mut session, order) = submarine_go_to_fixture(true);
        assert!(matches!(
            session.command(order.clone(), 2_000).as_slice(),
            [Effect::Send { .. }]
        ));
        let yaw = submarine_ack(&mut session, 192, 0, 3_000);
        let [Effect::Send { bytes, .. }] = yaw.as_slice() else {
            panic!("{yaw:?}")
        };
        let frames = crate::framing::split(bytes).frames;
        let Message::CommandLong {
            command,
            param1,
            param2,
            ..
        } = decode(frames[0]).unwrap().message
        else {
            panic!("not yaw")
        };
        assert_eq!(command, 115);
        assert!((param1 - 49.1).abs() < 0.5, "bearing {param1}");
        assert_eq!(param2, 0.0);
        assert_eq!(
            &bytes[18..26],
            &[0; 8],
            "absolute yaw with no forced direction"
        );
        // Keep the original command reserved while heading is pending.
        assert!(matches!(
            session.command(order, 3_100).as_slice(),
            [Effect::CommandDone {
                outcome: CommandOutcome::NotSent(_),
                ..
            }]
        ));
        assert!(
            matches!(submarine_ack(&mut session, 115, 0, 4_000).as_slice(),
            [Effect::CommandDone { receipt_id, outcome: CommandOutcome::Accepted, attempts: 2 }] if receipt_id == "goto")
        );
    }

    #[test]
    fn submarine_heading_choices_capture_current_yaw_and_validate_explicit_bearings() {
        use uxduck_core::domain::command::GoToHeading;
        for (heading, expected) in [
            (GoToHeading::FaceDestination, 49.1),
            (GoToHeading::KeepCurrent, 91.0),
            (GoToHeading::Set { degrees: 125.0 }, 125.0),
        ] {
            // Explicit heading choices also work on a real ArduSub; legacy
            // omitted headings keep the existing real-vehicle behavior.
            let (mut session, mut order) = submarine_go_to_fixture(false);
            if let Command::GoTo {
                heading: choice, ..
            } = &mut order.command
            {
                *choice = Some(heading);
            }
            let sent = session.command(order, 2_000);
            assert!(matches!(sent.as_slice(), [Effect::Send { .. }]), "{sent:?}");
            let yaw = submarine_ack(&mut session, 192, 0, 3_000);
            let [Effect::Send { bytes, .. }] = yaw.as_slice() else {
                panic!("{yaw:?}")
            };
            let frames = crate::framing::split(bytes).frames;
            let Message::CommandLong {
                command: 115,
                param1,
                ..
            } = decode(frames[0]).unwrap().message
            else {
                panic!("not yaw")
            };
            assert!((param1 - expected).abs() < 0.5, "{heading:?}: {param1}");
        }
        for degrees in [-1.0, 361.0, f64::NAN] {
            let (mut session, mut order) = submarine_go_to_fixture(true);
            if let Command::GoTo { heading, .. } = &mut order.command {
                *heading = Some(GoToHeading::Set { degrees });
            }
            assert!(matches!(session.command(order, 2_000).as_slice(),
                [Effect::CommandDone { outcome: CommandOutcome::NotSent(why), .. }] if why.contains("0 to 360")));
        }
        // Keeping a heading must never silently use an old target heading.
        let (mut session, mut order) = submarine_go_to_fixture(true);
        session.vehicles.values_mut().next().unwrap().heading = None;
        if let Command::GoTo { heading, .. } = &mut order.command {
            *heading = Some(GoToHeading::KeepCurrent);
        }
        assert!(matches!(session.command(order, 2_000).as_slice(),
            [Effect::CommandDone { outcome: CommandOutcome::NotSent(why), .. }] if why.contains("fresh heading")));
        // A depth-only default faces the captured heading, not a stale yaw target.
        let (mut session, mut order) = submarine_go_to_fixture(true);
        if let Command::GoTo {
            latitude_deg,
            longitude_deg,
            ..
        } = &mut order.command
        {
            *latitude_deg = 37.763_932;
            *longitude_deg = -122.385_804;
        }
        session.command(order, 2_000);
        let yaw = submarine_ack(&mut session, 192, 0, 3_000);
        let [Effect::Send { bytes, .. }] = yaw.as_slice() else {
            panic!("{yaw:?}")
        };
        let frames = crate::framing::split(bytes).frames;
        assert!(matches!(
            decode(frames[0]).unwrap().message,
            Message::CommandLong {
                command: 115,
                param1: 91.0,
                ..
            }
        ));
    }

    #[test]
    fn submarine_go_to_rejections_and_heading_timeouts_are_visible() {
        let (mut session, order) = submarine_go_to_fixture(true);
        session.command(order, 2_000);
        assert!(matches!(
            submarine_ack(&mut session, 192, 2, 3_000).as_slice(),
            [Effect::CommandDone {
                outcome: CommandOutcome::Rejected(_),
                attempts: 1,
                ..
            }]
        ));
        let (mut session, order) = submarine_go_to_fixture(true);
        session.command(order, 2_000);
        submarine_ack(&mut session, 192, 0, 3_000);
        assert!(
            matches!(submarine_ack(&mut session, 115, 2, 4_000).as_slice(),
            [Effect::CommandDone { outcome: CommandOutcome::Rejected(why), attempts: 2, .. }] if why.contains("destination accepted; heading refused"))
        );
        let (mut session, order) = submarine_go_to_fixture(true);
        session.config.command_attempts = 1;
        session.command(order, 2_000);
        submarine_ack(&mut session, 192, 0, 3_000);
        let effects = session.tick(3_000 + session.config.command_timeout_us);
        assert!(effects.iter().any(|effect| matches!(effect,
            Effect::CommandDone { outcome: CommandOutcome::Rejected(why), attempts: 2, .. } if why.contains("heading command timed out"))));
    }

    #[test]
    fn submarine_heading_requires_fresh_position_and_leaves_real_vehicles_alone() {
        for position in [None, Some((37.7, -122.4, 0))] {
            let (mut session, order) = submarine_go_to_fixture(true);
            session.vehicles.values_mut().next().unwrap().position = position;
            assert!(matches!(session.command(order, 6_000_000).as_slice(),
                [Effect::CommandDone { outcome: CommandOutcome::NotSent(why), .. }] if why.contains("fresh fused position")));
        }
        let (mut session, order) = submarine_go_to_fixture(false);
        session.vehicles.values_mut().next().unwrap().position = None;
        session.command(order, 2_000);
        assert!(matches!(
            submarine_ack(&mut session, 192, 0, 3_000).as_slice(),
            [Effect::CommandDone {
                outcome: CommandOutcome::Accepted,
                attempts: 1,
                ..
            }]
        ));
        assert_eq!(heading_to(37.7, -122.4, 37.7, -122.4), None);
        assert!((heading_to(0.0, 179.9, 0.0, -179.9).unwrap() - 90.0).abs() < 0.01);
    }

    #[test]
    fn a_disarm_cancels_a_simulated_submarines_pending_heading() {
        for yaw_sent in [false, true] {
            let (mut session, order) = submarine_go_to_fixture(true);
            session.command(order.clone(), 2_000);
            if yaw_sent {
                submarine_ack(&mut session, 192, 0, 3_000);
            }
            let disarm = CommandOrder {
                command: Command::Disarm,
                receipt_id: "stop".into(),
                ..order
            };
            session.command(disarm, 4_000);
            if yaw_sent {
                assert!(submarine_ack(&mut session, 115, 0, 5_000).is_empty());
                assert!(session.commands.iter().all(|p| p.command != 115));
            } else {
                assert!(matches!(
                    submarine_ack(&mut session, 192, 0, 5_000).as_slice(),
                    [Effect::CommandDone {
                        outcome: CommandOutcome::Accepted,
                        ..
                    }]
                ));
            }
        }
    }

    #[test]
    fn px4_takes_off_to_home_plus_the_height_and_shows_its_modes() {
        use crate::sim::{global_position_frame, px4_heartbeat_frame};
        let mut session = Session::new(Config::default());
        // AUTO.LOITER, as PX4 holds on the ground.
        let seen = session.ingest("l1", PEER, &px4_heartbeat_frame(1, 0, 0x0304_0000), 0);
        assert_eq!(vehicles(&seen)[0].mode.as_deref(), Some("Hold"));
        let vid = session.vehicle_at("l1", PEER, 1).unwrap().to_owned();
        let take_off = |receipt: &str| CommandOrder {
            receipt_id: receipt.into(),
            vehicle_id: vid.clone(),
            command: Command::take_off(20.0),
            force: false,
        };
        let refused = session.command(take_off("r1"), 10);
        assert!(matches!(
            refused.as_slice(),
            [Effect::CommandDone { outcome: CommandOutcome::NotSent(why), .. }] if why.contains("home")
        ));

        // On the ground 584 m above sea level.
        session.ingest("l1", PEER, &global_position_frame(1, 1, 584_000, 0), 20);
        let sent = session.command(take_off("r2"), 30);
        let [Effect::Send { bytes, .. }] = sent.as_slice() else {
            panic!("{sent:?}");
        };
        let frames = crate::framing::split(bytes).frames;
        let Some(decoded) = decode(frames[0]) else {
            panic!("not a frame")
        };
        let Message::CommandLong {
            command, param7, ..
        } = decoded.message
        else {
            panic!("{:?}", decoded.message);
        };
        assert_eq!(command, 22);
        assert!((param7 - 604.0).abs() < 0.01, "{param7}");
    }

    #[test]
    fn a_place_above_the_sea_or_the_terrain_goes_in_its_frame_and_the_fake_vehicle_keeps_it() {
        use uxduck_core::domain::telemetry::AltitudeFrame;
        // Home is 584 m above the sea, the ground under it 4 m lower.
        for (frame, altitude_m, wire_frame, above_home_m) in [
            (AltitudeFrame::Amsl, 634.0, 5, 50.0),
            (AltitudeFrame::AboveGround, 24.0, 11, 20.0),
        ] {
            let mut session = Session::new(Config::default());
            let mut fake = FakeVehicle::new(1, 0xABCD);
            let first = session.ingest("l1", PEER, &fake.heartbeat(), 0);
            converse(&mut session, &mut fake, first, 0);
            let vid = session.vehicle_at("l1", PEER, 1).unwrap().to_owned();
            let _ = fake.telemetry(10.0);
            let sent = session.command(
                CommandOrder {
                    receipt_id: "r1".into(),
                    vehicle_id: vid,
                    command: Command::GoTo {
                        latitude_deg: -35.36,
                        longitude_deg: 149.17,
                        altitude_m,
                        altitude_frame: frame,
                        heading: None,
                    },
                    force: false,
                },
                20_000_000,
            );
            let [Effect::Send { bytes, .. }] = sent.as_slice() else {
                panic!("{sent:?}");
            };
            let decoded = decode(crate::framing::split(bytes).frames[0]).expect("a frame");
            let Message::CommandInt {
                frame: sent_frame,
                z,
                ..
            } = decoded.message
            else {
                panic!("{:?}", decoded.message);
            };
            assert_eq!(sent_frame, wire_frame);
            assert!((f64::from(z) - altitude_m).abs() < 0.01, "{z}");
            assert_eq!(fake.answer(bytes).len(), 1);
            let height = fake
                .telemetry(200.0)
                .iter()
                .find_map(|frame| match decode(frame)?.message {
                    Message::GlobalPositionInt { relative_alt, .. } => {
                        Some(f64::from(relative_alt) / 1000.0)
                    }
                    _ => None,
                })
                .expect("a height");
            assert!((height - above_home_m).abs() < 0.1, "{frame:?}: {height}");
        }
    }

    #[test]
    fn a_place_goes_as_a_command_int_and_the_fake_vehicle_flies_there() {
        let mut session = Session::new(Config::default());
        let mut fake = FakeVehicle::new(1, 0xABCD);
        let first = session.ingest("l1", PEER, &fake.heartbeat(), 0);
        converse(&mut session, &mut fake, first, 0);
        let vid = session.vehicle_at("l1", PEER, 1).unwrap().to_owned();
        let position = |fake: &mut FakeVehicle, t_s: f64| -> (f64, f64) {
            fake.telemetry(t_s)
                .iter()
                .find_map(|frame| match decode(frame)?.message {
                    Message::GlobalPositionInt { lat, lon, .. } => {
                        Some((f64::from(lat) / 1e7, f64::from(lon) / 1e7))
                    }
                    _ => None,
                })
                .expect("a position")
        };
        // Flying its circle when the place comes.
        let start = position(&mut fake, 10.0);
        let to = (-35.36, 149.17);
        let sent = session.command(
            CommandOrder {
                receipt_id: "r1".into(),
                vehicle_id: vid,
                command: Command::GoTo {
                    latitude_deg: to.0,
                    longitude_deg: to.1,
                    altitude_m: 30.0,
                    altitude_frame: uxduck_core::domain::telemetry::AltitudeFrame::AboveHome,
                    heading: None,
                },
                force: false,
            },
            20_000_000,
        );
        let [Effect::Send { bytes, .. }] = sent.as_slice() else {
            panic!("{sent:?}");
        };
        let frames = crate::framing::split(bytes).frames;
        let decoded = decode(frames[0]).expect("a frame");
        let Message::CommandInt {
            command,
            frame,
            x,
            y,
            z,
            ..
        } = decoded.message
        else {
            panic!("{:?}", decoded.message);
        };
        assert_eq!(
            (command, frame, x, y),
            (192, 6, -353_600_000, 1_491_700_000)
        );
        assert!((z - 30.0).abs() < 0.01, "{z}");

        // The fake vehicle takes it, goes GUIDED, and the receipt is accepted.
        let replies = fake.answer(bytes);
        assert_eq!((replies.len(), fake.mode), (1, 4));
        let done = session.ingest("l1", PEER, &replies[0], 21_000_000);
        assert!(
            done.iter().any(|effect| matches!(
                effect,
                Effect::CommandDone {
                    outcome: CommandOutcome::Accepted,
                    ..
                }
            )),
            "{done:?}"
        );

        // Then it flies straight there and holds.
        let metres_to = |(lat, lon): (f64, f64)| {
            ((lat - to.0) * 111_320.0).hypot((lon - to.1) * 111_320.0 * to.0.to_radians().cos())
        };
        let on_the_way = position(&mut fake, 30.0);
        assert!(metres_to(on_the_way) < metres_to(start) - 100.0);
        let arrived = position(&mut fake, 200.0);
        assert!(metres_to(arrived) < 0.1, "{arrived:?}");
        assert_eq!(position(&mut fake, 260.0), arrived);
        // Sent to LOITER, it leaves the place.
        let loiter = crate::sim::command_long_frame(
            255,
            1,
            1,
            1,
            176,
            [1.0, 5.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            0,
        );
        fake.answer(&loiter);
        assert_eq!(fake.mode, 5);
        assert!(metres_to(position(&mut fake, 261.0)) > 100.0);
    }

    #[test]
    fn a_refusal_a_timeout_and_a_vehicle_with_no_address() {
        let config = Config::default();
        let mut session = Session::new(config);
        let mut fake = FakeVehicle::new(1, 0xABCD);
        fake.refuses_arm = true;
        session.ingest("l1", PEER, &fake.heartbeat(), 0);
        let vid = session.vehicle_at("l1", PEER, 1).unwrap().to_owned();
        let order = |receipt: &str| CommandOrder {
            receipt_id: receipt.into(),
            vehicle_id: vid.clone(),
            command: Command::Arm,
            force: false,
        };
        let sent = session.command(order("r1"), 0);
        let Effect::Send { bytes, .. } = &sent[0] else {
            panic!()
        };
        let done = session.ingest("l1", PEER, &fake.answer(bytes)[0], 10);
        assert!(matches!(
            done.as_slice(),
            [Effect::CommandDone { outcome: CommandOutcome::Rejected(reason), .. }] if reason == "Denied"
        ));

        // Silence: sent three times in all, then a timeout.
        session.command(order("r2"), 100);
        let mut transmissions = 1;
        let mut outcome = None;
        for step in 1..=10u64 {
            for effect in session.tick(100 + step * config.command_timeout_us) {
                match effect {
                    Effect::Send { .. } => transmissions += 1,
                    Effect::CommandDone {
                        outcome: o,
                        attempts,
                        ..
                    } => outcome = Some((o, attempts)),
                    _ => {}
                }
            }
        }
        assert_eq!(transmissions, 3);
        assert_eq!(outcome, Some((CommandOutcome::Timeout, 3)));

        let nowhere = session.command(
            CommandOrder {
                receipt_id: "r3".into(),
                vehicle_id: "unknown".into(),
                command: Command::Rtl,
                force: false,
            },
            0,
        );
        assert!(matches!(
            nowhere.as_slice(),
            [Effect::CommandDone {
                outcome: CommandOutcome::NotSent(_),
                attempts: 0,
                ..
            }]
        ));
    }

    #[test]
    fn telemetry_is_produced_and_coalesced_per_tick() {
        let mut session = Session::new(Config::default());
        let mut fake = FakeVehicle::new(1, 0xABCD);
        let first = session.ingest("l1", PEER, &fake.heartbeat(), 0);
        converse(&mut session, &mut fake, first, 0);
        assert!(
            session
                .ingest("l1", PEER, &fake.telemetry(0.0).concat(), 10)
                .is_empty(),
            "telemetry waits for the tick"
        );
        session.ingest("l1", PEER, &fake.telemetry(15.0).concat(), 20);
        let effects = session.tick(30);
        let telemetry: Vec<&VehicleTelemetry> = effects
            .iter()
            .filter_map(|e| {
                if let Effect::Telemetry(t) = e {
                    Some(t)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(telemetry.len(), 1);
        let t = telemetry[0];
        let sources: Vec<&str> = t.positions.iter().map(|p| p.source.as_str()).collect();
        assert_eq!(sources, ["fused", "gps"]);
        let fused = &t.positions[0];
        // A quarter of the way round: due east of home.
        assert!(
            (fused.latitude_deg - -35.363_261).abs() < 1e-5,
            "{}",
            fused.latitude_deg
        );
        assert!(fused.longitude_deg > 149.165_23);
        assert_eq!(
            fused.altitudes[1].frame,
            uxduck_core::domain::telemetry::AltitudeFrame::AboveHome
        );
        assert!((fused.altitudes[1].meters - 30.0).abs() < 1e-6);
        // The fake vehicle's terrain lies 4 m under home, so the fused
        // position gains a third altitude, above the ground.
        assert_eq!(fused.altitudes[2].frame, AltitudeFrame::AboveGround);
        assert!(
            (fused.altitudes[2].meters - 34.0).abs() < 1e-3,
            "{}",
            fused.altitudes[2].meters
        );
        assert!(
            (t.items.iter().find(|i| i.key == "terrain.altitude_msl_m"))
                .and_then(|i| i.value)
                .is_some_and(|v| (v - 580.0).abs() < 1e-3)
        );
        let heading = fused.heading_deg.unwrap_or_default();
        assert!((heading - 180.0).abs() < 0.1, "{heading}");
        let item = |key: &str| t.items.iter().find(|i| i.key == key).cloned();
        assert_eq!(item("gps.fix").and_then(|i| i.text).as_deref(), Some("3D"));
        assert_eq!(item("gps.satellites").and_then(|i| i.value), Some(14.0));
        assert!(
            item("battery.voltage_v")
                .and_then(|i| i.value)
                .is_some_and(|v| v > 12.0)
        );
        assert!(
            (item("attitude.roll_deg")
                .and_then(|i| i.value)
                .unwrap_or_default()
                - 12.0)
                .abs()
                < 0.01
        );
        assert_eq!(
            t.items
                .iter()
                .filter(|i| i.key == "throttle.output_pct")
                .count(),
            1,
            "two frames of one kind in a tick coalesce"
        );
        assert!(
            !session
                .tick(40)
                .iter()
                .any(|e| matches!(e, Effect::Telemetry(_))),
            "nothing new, nothing sent"
        );
    }

    /// Plays sends to the fake and its replies back until both are quiet;
    /// returns everything else that happened.
    fn converse(
        session: &mut Session,
        fake: &mut FakeVehicle,
        effects: Vec<Effect>,
        now_us: u64,
    ) -> Vec<Effect> {
        let mut pending = effects;
        let mut rest = Vec::new();
        for _ in 0..10_000 {
            if pending.is_empty() {
                break;
            }
            let mut next = Vec::new();
            for effect in pending {
                match effect {
                    Effect::Send { bytes, .. } => {
                        for reply in fake.answer(&bytes) {
                            next.extend(session.ingest("l1", PEER, &reply, now_us));
                        }
                    }
                    other => rest.push(other),
                }
            }
            pending = next;
        }
        rest
    }

    fn identified(fake: &mut FakeVehicle) -> (Session, String) {
        let mut session = Session::new(Config::default());
        let first = session.ingest("l1", PEER, &fake.heartbeat(), 0);
        converse(&mut session, fake, first, 0);
        let vid = session
            .vehicle_at("l1", PEER, fake.system_id)
            .unwrap()
            .to_owned();
        (session, vid)
    }

    #[test]
    fn parameters_come_by_ftp_with_their_defaults() {
        let mut fake = FakeVehicle::new(1, 0xABCD);
        // Changed from its default before the download.
        let rtl = fake
            .parameters
            .iter()
            .position(|(n, _, _)| n == "RTL_ALT")
            .unwrap();
        fake.parameters[rtl].1 = 3000.0;
        let (mut session, vid) = identified(&mut fake);
        let started = session.download_parameters(&vid, 10);
        let done = converse(&mut session, &mut fake, started, 10);
        let Some(Effect::Parameters {
            result: Ok(all),
            defaults,
            ..
        }) = done.first()
        else {
            panic!("{done:?}");
        };
        assert_eq!(all.len(), crate::sim::FAKE_PARAMETERS.len());
        let value = |name: &str| all.iter().find(|p| p.name == name).unwrap().value;
        assert!((value("RTL_ALT") - 3000.0).abs() < f32::EPSILON);
        let default = crate::sim::FAKE_PARAMETERS
            .iter()
            .find(|(n, _, _)| *n == "RTL_ALT")
            .unwrap()
            .1;
        assert!((defaults["RTL_ALT"] - default).abs() < f32::EPSILON);
        assert_eq!(defaults.len(), all.len(), "every one with its default");
    }

    #[test]
    fn another_ground_station_id_is_sent_as_and_answered_to() {
        let mut fake = FakeVehicle::new(1, 0xABCD);
        let mut session = Session::new(Config {
            gcs_system_id: 42,
            ..Config::default()
        });
        let first = session.ingest("l1", PEER, &fake.heartbeat(), 0);
        assert!(
            sends(&first)
                .iter()
                .all(|bytes| decode(bytes).unwrap().system_id == 42),
            "everything goes from 42"
        );
        converse(&mut session, &mut fake, first, 0);
        let vid = session.vehicle_at("l1", PEER, 1).unwrap().to_owned();

        // The fake answers 42, and the session takes the answer.
        let order = |receipt: &str| CommandOrder {
            receipt_id: receipt.into(),
            vehicle_id: vid.clone(),
            command: Command::Arm,
            force: false,
        };
        let sent = session.command(order("r1"), 10);
        let [Effect::Send { bytes, .. }] = sent.as_slice() else {
            panic!("{sent:?}");
        };
        let acks = fake.answer(bytes);
        let Some(Message::CommandAck { target_system, .. }) = decode(&acks[0]).map(|d| d.message)
        else {
            panic!("not an ack");
        };
        assert_eq!(target_system, 42);
        let done = session.ingest("l1", PEER, &acks[0], 20);
        assert!(matches!(
            done.as_slice(),
            [Effect::CommandDone {
                outcome: CommandOutcome::Accepted,
                ..
            }]
        ));

        // An answer to 255, another ground station, is not ours.
        let sent = session.command(order("r2"), 30);
        let others = crate::sim::command_ack_frame(1, 1, (255, 190), 0, 400, 0);
        assert!(session.ingest("l1", PEER, &others, 40).is_empty());
        assert!(!sent.is_empty());

        // Parameters come by FTP, which checks who it is for.
        let started = session.download_parameters(&vid, 50);
        let done = converse(&mut session, &mut fake, started, 50);
        assert!(
            matches!(done.first(), Some(Effect::Parameters { result: Ok(_), .. })),
            "{done:?}"
        );

        // Changed while running, the next frame goes from the new id.
        session.set_gcs_system_id(7);
        let mode = CommandOrder {
            command: Command::SetMode {
                mode: "LOITER".into(),
            },
            ..order("r3")
        };
        let sent = session.command(mode, 60);
        let Some(Effect::Send { bytes, .. }) = sent.first() else {
            panic!("{sent:?}");
        };
        assert_eq!(decode(bytes).unwrap().system_id, 7);
    }

    #[test]
    fn the_parameter_file_comes_in_one_burst() {
        let mut fake = FakeVehicle::new(1, 0xABCD);
        let file = fake.param_pck().len();
        assert!(
            file > fake.parameters.len() * 12,
            "longer than ArduPilot's estimate, {file} bytes"
        );
        let (mut session, vid) = identified(&mut fake);
        let started = session.download_parameters(&vid, 10);
        let done = converse(&mut session, &mut fake, started, 10);
        assert!(
            matches!(done.first(), Some(Effect::Parameters { result: Ok(_), .. })),
            "{done:?}"
        );
        // One burst brings it all, its end included; nothing read singly.
        assert_eq!((fake.files.bursts, fake.files.reads), (1, 0));
    }

    #[test]
    fn a_download_after_a_restart_comes_by_ftp_again() {
        let mut fake = FakeVehicle::new(1, 0xABCD);
        let (mut session, vid) = identified(&mut fake);
        let started = session.download_parameters(&vid, 10);
        let done = converse(&mut session, &mut fake, started, 10);
        assert!(
            matches!(done.first(), Some(Effect::Parameters { result: Ok(_), .. })),
            "{done:?}"
        );
        fake.reboot();
        let started = session.download_parameters(&vid, 5_000_000);
        let done = converse(&mut session, &mut fake, started, 5_000_000);
        assert!(
            matches!(done.first(), Some(Effect::Parameters { result: Ok(_), .. })),
            "second: {done:?}"
        );
    }

    #[test]
    fn a_param_pck_reads_back_as_written() {
        use crate::transfers::{encode_param_pck, parse_param_pck};
        // Enough parameters to cross a 512-byte block, names sharing prefixes.
        let params: Vec<_> = (0..80)
            .map(|i| {
                (
                    Param {
                        name: format!("WPNAV_PARAM_{i:02}"),
                        value: f32::from(u16::try_from(i).unwrap()) * 1.5,
                        kind: if i % 3 == 0 { 6 } else { 9 },
                    },
                    (i % 2 == 0).then_some(2.0),
                )
            })
            .collect();
        let back = parse_param_pck(&encode_param_pck(&params)).unwrap();
        assert_eq!(back.len(), params.len());
        for ((written, wdefault), (read, rdefault)) in params.iter().zip(&back) {
            assert_eq!(written.name, read.name);
            assert_eq!(written.kind, read.kind);
            assert!(
                (written.value.trunc() - read.value.trunc()).abs() < f32::EPSILON
                    || written.kind == 9
            );
            // A default where it differs; otherwise the value is its default.
            let expected = wdefault.filter(|d| (d - read.value).abs() > f32::EPSILON);
            assert_eq!(
                *rdefault,
                Some(expected.unwrap_or(read.value)),
                "{}",
                read.name
            );
        }
        assert!(parse_param_pck(&[0, 0, 0, 0, 0, 0]).is_err(), "not a pack");
    }

    #[test]
    fn parameters_download_through_a_lossy_link_and_a_write_is_confirmed() {
        let mut fake = FakeVehicle::new(1, 0xABCD);
        fake.drops_every = Some(5);
        fake.ftp = false;
        let (mut session, vid) = identified(&mut fake);
        let started = session.download_parameters(&vid, 10);
        assert!(
            converse(&mut session, &mut fake, started, 10).is_empty(),
            "no FTP"
        );
        // FTP unanswered: sent three times, then the list, which arrives with gaps.
        let mut now = 10;
        for _ in 0..3 {
            now += 1_600_000;
            let tick = session.tick(now);
            let out = converse(&mut session, &mut fake, tick, now);
            assert!(!out.iter().any(|e| matches!(e, Effect::Parameters { .. })));
        }
        // After the gap, the missing ones are asked for by index.
        let tick = session.tick(now + 1_100_000);
        let done = converse(&mut session, &mut fake, tick, now + 1_100_000);
        let Some(Effect::Parameters {
            result: Ok(all), ..
        }) = done.iter().find(|e| matches!(e, Effect::Parameters { .. }))
        else {
            panic!("{done:?}");
        };
        assert_eq!(all.len(), crate::sim::FAKE_PARAMETERS.len());
        assert_eq!(all[1].name, "SYSID_THISMAV");

        let write = |name: &str, value: f32| Param {
            name: name.into(),
            value,
            kind: crate::sim::REAL32,
        };
        let sent = session.set_parameter("o1", &vid, write("WPNAV_SPEED", 750.0), false, 2_000_000);
        let done = converse(&mut session, &mut fake, sent, 2_000_000);
        assert!(
            matches!(
                done.as_slice(),
                [Effect::ParamWritten { order_id, result: Ok(p), attempts: 1 }, Effect::ParamValue { .. }]
                    if order_id == "o1" && p.value == 750.0
            ),
            "{done:?}"
        );
        // A read-only one comes back unchanged.
        let sent =
            session.set_parameter("o2", &vid, write("FORMAT_VERSION", 1.0), false, 3_000_000);
        let done = converse(&mut session, &mut fake, sent, 3_000_000);
        assert!(matches!(
            done.first(),
            Some(Effect::ParamWritten { result: Ok(p), .. }) if p.value == 120.0
        ));
        // Silence: three tries, then a timeout.
        fake.ignores_commands = true;
        let mut silent = FakeVehicle::new(1, 0xABCD);
        silent.parameters.clear();
        session.set_parameter("o3", &vid, write("RTL_ALT", 3000.0), false, 4_000_000);
        let mut outcome = None;
        for step in 1..=5u64 {
            for effect in session.tick(4_000_000 + step * 1_600_000) {
                if let Effect::ParamWritten {
                    result, attempts, ..
                } = effect
                {
                    outcome = Some((result, attempts));
                }
            }
        }
        assert_eq!(outcome, Some((Err(Failure::Timeout), 3)));
    }

    /// Parameters written together: they wait for the tick, and eight or
    /// more go up as one file.
    fn write_together(
        session: &mut Session,
        fake: &mut FakeVehicle,
        vid: &str,
        writes: &[(&str, f32, u8)],
        now_us: u64,
    ) -> Vec<Effect> {
        // Sent back to back, as the engine sends them, before any answer.
        let mut sent = Vec::new();
        for (i, (name, value, kind)) in writes.iter().enumerate() {
            let param = Param {
                name: (*name).into(),
                value: *value,
                kind: *kind,
            };
            sent.extend(session.set_parameter(&format!("o{i}"), vid, param, true, now_us));
        }
        let mut done = converse(session, fake, sent, now_us);
        let tick = session.tick(now_us + 100_000);
        done.extend(converse(session, fake, tick, now_us + 100_000));
        done
    }

    #[test]
    fn writes_made_together_go_up_as_one_file_and_are_read_back() {
        let mut fake = FakeVehicle::new(1, 0xABCD);
        let (mut session, vid) = identified(&mut fake);
        let started = session.download_parameters(&vid, 10);
        converse(&mut session, &mut fake, started, 10);
        let writes: Vec<(&str, f32, u8)> = crate::sim::FAKE_PARAMETERS[3..14]
            .iter()
            .map(|(name, value, kind)| (*name, value + 1.0, *kind))
            .chain([("NOT_A_PARAMETER", 1.0, crate::sim::REAL32)])
            .collect();
        let done = write_together(&mut session, &mut fake, &vid, &writes, 1_000_000);
        let written: Vec<&Effect> = done
            .iter()
            .filter(|e| matches!(e, Effect::ParamWritten { .. }))
            .collect();
        assert_eq!(written.len(), writes.len(), "{done:?}");
        let confirmed = written
            .iter()
            .filter(|e| matches!(e, Effect::ParamWritten { result: Ok(_), .. }))
            .count();
        assert_eq!(confirmed, writes.len() - 1);
        assert!(done.iter().any(|e| matches!(
            e,
            Effect::ParamWritten { result: Err(Failure::Rejected(why)), .. }
                if why == "the vehicle has no parameter NOT_A_PARAMETER"
        )));
        // The read-back refreshes the whole set, defaults too.
        assert!(done.iter().any(|e| matches!(
            e,
            Effect::Parameters { result: Ok(all), defaults, .. }
                if all.len() == crate::sim::FAKE_PARAMETERS.len() && !defaults.is_empty()
        )));
        for (name, value, _) in &writes[..writes.len() - 1] {
            let held = fake.parameters.iter().find(|(n, _, _)| n == name).unwrap();
            assert!((held.1 - value).abs() < f32::EPSILON, "{name}");
        }
        // All twelve in one file, read back.
        assert_eq!(fake.files.param_uploads, 1);
        assert_eq!(fake.files.bursts, 2, "the download and the read-back");
    }

    #[test]
    fn a_few_writes_together_go_by_param_set() {
        let mut fake = FakeVehicle::new(1, 0xABCD);
        let (mut session, vid) = identified(&mut fake);
        let started = session.download_parameters(&vid, 10);
        converse(&mut session, &mut fake, started, 10);
        let writes = [
            ("RTL_ALT", 2000.0, crate::sim::INT32),
            ("WPNAV_SPEED", 800.0, crate::sim::REAL32),
            ("LAND_SPEED", 40.0, crate::sim::INT16),
        ];
        let done = write_together(&mut session, &mut fake, &vid, &writes, 1_000_000);
        let values = done
            .iter()
            .filter(|e| matches!(e, Effect::ParamValue { .. }))
            .count();
        assert_eq!(values, 3, "each answered by PARAM_VALUE: {done:?}");
        assert_eq!(fake.files.bursts, 1, "no read-back");
        assert_eq!(fake.files.param_uploads, 0);
    }

    #[test]
    fn writes_not_asked_for_by_file_go_at_once() {
        let mut fake = FakeVehicle::new(1, 0xABCD);
        let (mut session, vid) = identified(&mut fake);
        let started = session.download_parameters(&vid, 10);
        converse(&mut session, &mut fake, started, 10);
        let param = |name: &str| Param {
            name: name.into(),
            value: 1.0,
            kind: crate::sim::REAL32,
        };
        for (i, name) in ["WPNAV_SPEED", "WPNAV_ACCEL", "WPNAV_RADIUS"]
            .iter()
            .enumerate()
        {
            let sent = session.set_parameter(&format!("o{i}"), &vid, param(name), false, 20);
            assert!(!sent.is_empty(), "{name} sent at once");
        }
    }

    #[test]
    fn a_vehicle_without_parameters_by_file_gets_param_set_at_once() {
        let mut fake = FakeVehicle::new(1, 0xABCD);
        fake.ftp = false;
        let (mut session, vid) = identified(&mut fake);
        let param = |name: &str| Param {
            name: name.into(),
            value: 1.0,
            kind: crate::sim::REAL32,
        };
        let first = session.set_parameter("o1", &vid, param("WPNAV_SPEED"), true, 10);
        let second = session.set_parameter("o2", &vid, param("WPNAV_ACCEL"), true, 10);
        assert!(!first.is_empty() && !second.is_empty(), "both sent at once");
    }

    #[test]
    fn a_vehicles_files_are_listed_and_read_through_the_session() {
        let mut fake = FakeVehicle::new(1, 0xABCD);
        let (mut session, vid) = identified(&mut fake);
        let started = session.file(
            &vid,
            "l1",
            crate::files::Op::List {
                path: "/APM/LOGS".into(),
            },
            10,
        );
        let done = converse(&mut session, &mut fake, started, 10);
        assert!(done.iter().any(|e| matches!(
            e,
            Effect::File { event: crate::files::Event::Listed { entries, .. }, .. } if entries.len() == 4
        )), "{done:?}");
        let started = session.file(
            &vid,
            "r1",
            crate::files::Op::Read {
                path: "/APM/LOGS/00000002.BIN".into(),
                from: 0,
            },
            20,
        );
        let done = converse(&mut session, &mut fake, started, 20);
        let bytes: Vec<u8> = done
            .iter()
            .filter_map(|e| match e {
                Effect::File {
                    event: crate::files::Event::Data { bytes, .. },
                    ..
                } => Some(bytes.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(bytes, fake.files.files["/APM/LOGS/00000002.BIN"]);
        assert!(matches!(
            done.last(),
            Some(Effect::File {
                event: crate::files::Event::Done { result: Ok(()), .. },
                ..
            })
        ));
        let unknown = session.file(
            "nobody",
            "x",
            crate::files::Op::List { path: "/".into() },
            30,
        );
        assert!(matches!(
            unknown.as_slice(),
            [Effect::File {
                event: crate::files::Event::Done {
                    result: Err(Failure::NotSent(_)),
                    ..
                },
                ..
            }]
        ));
    }

    #[test]
    fn a_mission_goes_up_and_comes_back_down() {
        let mut fake = FakeVehicle::new(1, 0xABCD);
        let (mut session, vid) = identified(&mut fake);
        let item = |seq: u16, command: u16, z: f32| crate::decode::MissionItemInt {
            seq,
            frame: 3,
            command,
            current: false,
            autocontinue: true,
            params: [0.0; 4],
            x: -353_632_610,
            y: 1_491_652_300,
            z,
            mission_type: 0,
        };
        let plan = vec![
            item(0, 16, 0.0),
            item(1, 22, 20.0),
            item(2, 16, 30.0),
            item(3, 20, 0.0),
        ];
        // Progress as it goes (S127), apart from how each ends.
        let split = |effects: Vec<Effect>| -> (Vec<Effect>, Vec<Effect>) {
            effects
                .into_iter()
                .partition(|e| matches!(e, Effect::Progress { .. }))
        };
        let sent = session.upload_mission("u1", &vid, plan.clone(), 10);
        let (progress, done) = split(converse(&mut session, &mut fake, sent, 10));
        assert_eq!(
            done,
            [Effect::MissionUploaded {
                order_id: "u1".into(),
                result: Ok(())
            }]
        );
        assert!(
            matches!(
                progress.last(),
                Some(Effect::Progress {
                    transfer: TransferKind::MissionUpload,
                    done: 4,
                    total: Some(4),
                    attempt: 1,
                    ..
                })
            ),
            "{progress:?}"
        );
        assert_eq!(fake.mission, plan);
        let sent = session.download_mission(&vid, 20);
        let (progress, done) = split(converse(&mut session, &mut fake, sent, 20));
        assert!(
            matches!(done.as_slice(), [Effect::Mission { result: Ok(items), .. }] if *items == plan),
            "{done:?}"
        );
        assert!(progress.iter().any(|e| matches!(
            e,
            Effect::Progress {
                transfer: TransferKind::MissionDownload,
                done: 3,
                total: Some(4),
                ..
            }
        )));
        fake.mission_capacity = 2;
        let sent = session.upload_mission("u2", &vid, plan, 30);
        let (_, done) = split(converse(&mut session, &mut fake, sent, 30));
        assert!(matches!(
            done.as_slice(),
            [Effect::MissionUploaded { result: Err(Failure::Rejected(why)), .. }]
                if why == "No space for that many items"
        ));
    }

    #[test]
    fn a_fence_and_rally_points_go_up_and_come_back_as_their_own_kinds() {
        let mut fake = FakeVehicle::new(1, 0xABCD);
        let (mut session, vid) = identified(&mut fake);
        let corner = |seq: u16, x: i32, kind: u8, command: u16| crate::decode::MissionItemInt {
            seq,
            frame: 3,
            command,
            current: false,
            autocontinue: false,
            params: [3.0, 0.0, 0.0, 0.0],
            x,
            y: 1_491_652_300,
            z: 0.0,
            mission_type: kind,
        };
        let fence = vec![
            corner(0, -353_600_000, 1, 5001),
            corner(1, -353_700_000, 1, 5001),
            corner(2, -353_650_000, 1, 5001),
        ];
        let sent = session.upload_mission_part("f1", &vid, fence.clone(), 1, 10);
        let done: Vec<Effect> = converse(&mut session, &mut fake, sent, 10)
            .into_iter()
            .filter(|e| !matches!(e, Effect::Progress { .. }))
            .collect();
        assert_eq!(
            done,
            [Effect::MissionUploaded {
                order_id: "f1".into(),
                result: Ok(())
            }]
        );
        assert_eq!(fake.fence, fence);
        assert_eq!(fake.mission.len(), 1, "the mission is left as it was");
        let rally = vec![corner(0, -353_610_000, 2, 5100)];
        let sent = session.upload_mission_part("r1", &vid, rally.clone(), 2, 20);
        let _ = converse(&mut session, &mut fake, sent, 20);
        assert_eq!(fake.rally, rally);
        let sent = session.download_mission_part(&vid, 1, 30);
        let done: Vec<Effect> = converse(&mut session, &mut fake, sent, 30)
            .into_iter()
            .filter(|e| !matches!(e, Effect::Progress { .. }))
            .collect();
        assert!(
            matches!(done.as_slice(), [Effect::MissionPart { kind: 1, result: Ok(items), .. }] if *items == fence),
            "{done:?}"
        );
    }

    #[test]
    fn a_mission_transfer_is_cancelled_with_the_protocol_s_cancel() {
        let mut fake = FakeVehicle::new(1, 0xABCD);
        let (mut session, vid) = identified(&mut fake);
        let item = |seq: u16| crate::decode::MissionItemInt {
            seq,
            frame: 3,
            command: 16,
            current: false,
            autocontinue: true,
            params: [0.0; 4],
            x: -353_632_610,
            y: 1_491_652_300,
            z: 30.0,
            mission_type: 0,
        };
        // Started, but the vehicle has not answered yet.
        let _ = session.upload_mission("u1", &vid, vec![item(0), item(1)], 10);
        let cancelled = session.cancel_transfer(&vid, TransferKind::MissionUpload);
        assert!(cancelled.iter().any(|e| matches!(e, Effect::Send { .. })));
        assert!(cancelled.iter().any(|e| matches!(
            e,
            Effect::MissionUploaded { order_id, result: Err(Failure::Rejected(why)) }
                if order_id == "u1" && why == "cancelled"
        )));
        // Nothing left to cancel; a download may start now.
        assert!(
            session
                .cancel_transfer(&vid, TransferKind::MissionUpload)
                .is_empty()
        );
        assert!(
            session
                .cancel_transfer(&vid, TransferKind::Parameters)
                .is_empty()
        );
    }

    #[test]
    fn a_reboot_is_seen_in_time_since_boot_and_announced_after_identity() {
        let s = 1_000_000;
        let mut fake = FakeVehicle::new(1, 0xABCD);
        // Streaming on through the restart, as a vehicle with its own
        // `SRn_` rates does; a copter is asked again (below).
        fake.streams_on_request = false;
        let (mut session, vid) = identified(&mut fake);
        let booted = |effects: &[Effect]| {
            effects
                .iter()
                .filter_map(|e| match e {
                    Effect::Booted {
                        boot_instant_us, ..
                    } => Some(*boot_instant_us),
                    _ => None,
                })
                .collect::<Vec<u64>>()
        };
        // Ten seconds into its run, as the daemon first hears it.
        session.ingest("l1", PEER, &fake.telemetry(10.0).concat(), 100 * s);
        let first = session.tick(100 * s);
        assert_eq!(booted(&first), [90 * s], "announced once identified");
        session.ingest("l1", PEER, &fake.telemetry(12.0).concat(), 102 * s);
        assert!(booted(&session.tick(102 * s)).is_empty(), "same boot");

        fake.reboot();
        let after = session.ingest("l1", PEER, &fake.telemetry(13.0).concat(), 103 * s);
        assert_eq!(booted(&after), [102 * s]);
        let rates = commands(&after)
            .iter()
            .filter(|(command, _)| *command == 511)
            .count();
        assert_eq!(rates, STREAM_RATES.len(), "the whole rate table again");
        let _ = vid;
    }

    #[test]
    fn a_copter_that_restarts_forgets_its_rates_and_is_asked_again() {
        // ArduPilot keeps message intervals in memory only, and a copter's
        // own rates are all 0: after a restart it streams nothing. The
        // silence brings back the reconciled rows, their time since boot
        // shows the restart, and that brings back the whole table, the rows
        // sent only while set (wind, the rangefinder) too.
        let s = 1_000_000;
        let mut fake = FakeVehicle::new(1, 0xABCD);
        let (mut session, _) = identified(&mut fake);
        let mut restarted = false;
        let mut keys = std::collections::BTreeSet::new();
        for tick in 1..=120u64 {
            let now = tick * s / 2;
            if tick == 60 {
                fake.reboot();
                keys.clear();
            }
            #[allow(clippy::cast_precision_loss, reason = "test times are small")]
            let frames = fake.telemetry(now as f64 / 1e6).concat();
            let mut effects = session.ingest("l1", PEER, &fake.heartbeat(), now);
            effects.extend(session.ingest("l1", PEER, &frames, now));
            effects.extend(session.tick(now));
            for effect in converse(&mut session, &mut fake, effects, now) {
                match effect {
                    Effect::Booted {
                        restarted: true, ..
                    } => restarted = true,
                    Effect::Telemetry(t) => keys.extend(t.items.into_iter().map(|i| i.key)),
                    _ => {}
                }
            }
        }
        assert!(restarted, "the restart is seen");
        for key in [
            "attitude.roll_deg",
            "wind.speed_mps",
            "rangefinder.distance_m",
        ] {
            assert!(keys.contains(key), "{key} not back: {keys:?}");
        }
    }

    #[test]
    fn a_vehicle_paused_while_the_computer_slept_has_not_restarted() {
        let s = 1_000_000;
        let mut fake = FakeVehicle::new(1, 0xABCD);
        fake.streams_on_request = false;
        let (mut session, _) = identified(&mut fake);
        let booted =
            |effects: &[Effect]| effects.iter().any(|e| matches!(e, Effect::Booted { .. }));
        session.ingest("l1", PEER, &fake.telemetry(10.0).concat(), 100 * s);
        assert!(booted(&session.tick(100 * s)));
        // An hour asleep: the simulator stopped with the computer, so its
        // time since boot carries on from 10 s.
        let woke = session.ingest("l1", PEER, &fake.telemetry(10.5).concat(), 3_700 * s);
        assert!(!booted(&woke), "paused, not restarted");
        assert!(!booted(&session.tick(3_700 * s)));
        // It goes on from its new boot instant, and a real restart is still seen.
        session.ingest("l1", PEER, &fake.telemetry(12.0).concat(), 3_701 * s);
        assert!(!booted(&session.tick(3_701 * s)));
        fake.reboot();
        assert!(booted(&session.ingest(
            "l1",
            PEER,
            &fake.telemetry(1.0).concat(),
            3_702 * s
        )));
    }

    #[test]
    fn manual_control_reaches_the_vehicle_and_its_absence_is_a_failsafe() {
        let mut fake = FakeVehicle::new(1, 0xABCD);
        let (mut session, vid) = identified(&mut fake);
        let _ = fake.telemetry(1.0);
        let download = session.download_parameters(&vid, 0);
        converse(&mut session, &mut fake, download, 1);
        let sent = session.manual_control(&vid, [100, -200, 500, 0]);
        converse(&mut session, &mut fake, sent, 10);
        assert_eq!(fake.manual, Some([100, -200, 500, 0]));
        assert!(session.manual_control("nobody", [0; 4]).is_empty());
        let _ = fake.telemetry(2.0);
        assert_eq!(fake.mode, 0, "still flown");
        let _ = fake.telemetry(1.0 + crate::sim::FAILSAFE_S + 0.1);
        assert_eq!(fake.mode, 6, "RTL once the sticks go quiet");
    }

    #[test]
    fn rc_overrides_include_auxiliary_and_release_only_our_channels() {
        let mut fake = FakeVehicle::new(1, 0xABC);
        let (mut session, vid) = identified(&mut fake);
        let download = session.download_parameters(&vid, 0);
        converse(&mut session, &mut fake, download, 1);
        let sent = session.pilot_control(&vid, [1000, -1000, 500, 1000], &[(5, -1000), (9, 1000)]);
        let [Effect::Send { bytes, .. }] = sent.as_slice() else {
            panic!("no pilot frame");
        };
        let message = decode(bytes).unwrap();
        assert_eq!(message.system_id, crate::GCS_SYSTEM_ID);
        let Message::RcChannelsOverride { target, channels } = message.message else {
            panic!("wrong pilot message");
        };
        assert_eq!(target, 1);
        assert_eq!(
            &channels[..9],
            &[1000, 1000, 1500, 2000, 1000, 65535, 65535, 65535, 2000]
        );
        let released = session.release_control(&vid);
        let [Effect::Send { bytes, .. }] = released.as_slice() else {
            panic!("no release");
        };
        let Message::RcChannelsOverride { channels, .. } = decode(bytes).unwrap().message else {
            panic!("wrong release");
        };
        assert_eq!(&channels[..9], &[0, 0, 0, 0, 0, 65535, 65535, 65535, 65534]);
        assert!(session.release_control(&vid).is_empty());
    }

    #[test]
    fn a_new_control_session_downloads_calibration_before_emitting_pwm() {
        let mut fake = FakeVehicle::new(1, 0xABCD);
        let (mut session, vid) = identified(&mut fake);
        let first = session.manual_control(&vid, [0, 0, 500, 0]);
        assert!(!first.iter().any(|effect| matches!(effect, Effect::Send {bytes,..} if matches!(decode(bytes).map(|d|d.message),Some(Message::RcChannelsOverride {..})))));
        converse(&mut session, &mut fake, first, 1);
        let calibrated = session.manual_control(&vid, [0, 0, 500, 0]);
        assert!(calibrated.iter().any(|effect| matches!(effect, Effect::Send {bytes,..} if matches!(decode(bytes).map(|d|d.message),Some(Message::RcChannelsOverride {..})))));
    }

    #[test]
    fn px4_keeps_normalized_manual_control() {
        let mut session = Session::new(Config::default());
        session.ingest("l1", PEER, &crate::sim::px4_heartbeat_frame(1, 0, 0), 0);
        let vid = session.vehicle_at("l1", PEER, 1).unwrap().to_owned();
        let sent = session.manual_control(&vid, [10, 20, 500, 30]);
        let [Effect::Send { bytes, .. }] = sent.as_slice() else {
            panic!("no pilot frame");
        };
        assert!(matches!(
            decode(bytes).unwrap().message,
            Message::ManualControl {
                sticks: [10, 20, 500, 30],
                ..
            }
        ));
    }

    #[test]
    fn pilot_method_selects_the_wire_protocol_for_each_firmware_and_releases_rc() {
        use uxduck_core::domain::settings::PilotInputMethod::{
            Automatic, ManualControl, RcChannelsOverride,
        };
        for (autopilot, kind, default_manual) in [
            ("Ardupilotmega", "Quadrotor", false),
            ("Ardupilotmega", "FixedWing", false),
            ("Ardupilotmega", "GroundRover", false),
            ("Ardupilotmega", "SurfaceBoat", false),
            ("Ardupilotmega", "Submarine", true),
            ("Ardupilotmega", "Airship", false),
            ("Px4", "Quadrotor", true),
        ] {
            for method in [Automatic, ManualControl, RcChannelsOverride] {
                let mut fake = FakeVehicle::new(1, 0xABCD);
                fake.parameters.extend(
                    [
                        ("RC_MAP_ROLL", 1.0),
                        ("RC_MAP_PITCH", 2.0),
                        ("RC_MAP_THROTTLE", 3.0),
                        ("RC_MAP_YAW", 4.0),
                    ]
                    .map(|(name, value)| (name.into(), value, crate::sim::REAL32)),
                );
                let (mut session, vid) = identified(&mut fake);
                let download = session.download_parameters(&vid, 0);
                converse(&mut session, &mut fake, download, 1);
                let key = session.find(&vid).unwrap();
                let observation = &mut session.vehicles.get_mut(&key).unwrap().observation;
                observation.autopilot = autopilot.into();
                observation.vehicle_type = kind.into();
                let sent =
                    session.pilot_control_with_method(&vid, [1000, -1000, 500, 1000], &[], method);
                if kind == "Airship" && method == ManualControl {
                    assert!(sent.is_empty());
                    continue;
                }
                let [Effect::Send { bytes, .. }] = sent.as_slice() else {
                    panic!("{autopilot}/{kind}/{method:?}: {sent:?}");
                };
                let manual = method == ManualControl || (method == Automatic && default_manual);
                let decoded = decode(bytes).unwrap().message;
                assert_eq!(
                    matches!(decoded, Message::ManualControl { .. }),
                    manual,
                    "{autopilot}/{kind}/{method:?}"
                );
                if manual {
                    assert!(session.release_control(&vid).is_empty());
                } else {
                    let Message::RcChannelsOverride { channels, .. } = decoded else {
                        panic!("wrong wire protocol");
                    };
                    if kind == "Submarine" {
                        assert_eq!(&channels[..6], &[65535, 65535, 1500, 2000, 2000, 1000]);
                    }
                    if autopilot == "Px4" {
                        assert_eq!(&channels[..5], &[1000, 2000, 1500, 2000, 0]);
                        assert!(
                            session.release_control(&vid).is_empty(),
                            "PX4 stops publishing; no ArduPilot release sentinel"
                        );
                    } else {
                        let released = session.release_control(&vid);
                        assert_eq!(released.len(), 1);
                        assert!(session.release_control(&vid).is_empty());
                    }
                }
            }
        }
    }

    #[test]
    fn junk_is_counted_not_fatal() {
        let mut session = Session::new(Config::default());
        assert!(session.ingest("l1", PEER, &[1, 2, 3], 0).is_empty());
        assert_eq!(session.dropped, 1);
        assert_eq!(session.peers("l1"), [PEER]);
        assert_eq!(session.remove_link("l1").len(), 0);
    }

    #[test]
    fn a_camera_is_asked_for_its_streams_and_its_answer_names_the_vehicle() {
        let mut session = Session::new(Config::default());
        let mut vehicle = crate::sim::FakeVehicle::new(1, 0x1234);
        vehicle.camera = Some("rtsp://192.168.144.25:8554/main".into());
        let heartbeat = vehicle.heartbeat();
        session.ingest("l1", PEER, &heartbeat, 0);
        let camera = vehicle.camera_heartbeat().expect("a camera");
        let effects = session.ingest("l1", PEER, &camera, 10);
        let asks: Vec<&Vec<u8>> = effects
            .iter()
            .filter_map(|e| match e {
                Effect::Send { bytes, .. } => Some(bytes),
                _ => None,
            })
            .filter(|bytes| {
                matches!(
                    crate::decode::decode(bytes).map(|d| d.message),
                    Some(Message::CommandLong { command: 512, param1, .. }) if (param1 - 269.0).abs() < 0.5
                )
            })
            .collect();
        assert_eq!(asks.len(), 1);
        let mut streams = Vec::new();
        for reply in vehicle.answer(asks[0]) {
            for effect in session.ingest("l1", PEER, &reply, 20) {
                if let Effect::VideoStream {
                    uri, component_id, ..
                } = effect
                {
                    streams.push((uri, component_id));
                }
            }
        }
        assert_eq!(
            streams,
            [("rtsp://192.168.144.25:8554/main".to_owned(), 100)]
        );
        // Answered: not asked again.
        let later = session.tick(10_000_000);
        assert!(!later.iter().any(|e| matches!(e, Effect::Send { bytes, .. }
            if matches!(crate::decode::decode(bytes).map(|d| d.message),
                Some(Message::CommandLong { command: 512, param1, .. }) if (param1 - 269.0).abs() < 0.5))));
    }

    #[test]
    fn every_item_the_core_produces_says_its_unit_in_its_key() {
        let mut session = Session::new(Config::default());
        let mut vehicle = crate::sim::FakeVehicle::new(1, 0x1);
        let mut keys = std::collections::BTreeSet::new();
        let first = session.ingest("l1", PEER, &vehicle.heartbeat(), 0);
        converse(&mut session, &mut vehicle, first, 0);
        for step in 1..20u64 {
            for frame in vehicle.telemetry(f64::from(u32::try_from(step).unwrap_or(0)) * 0.2) {
                session.ingest("l1", PEER, &frame, step * 200_000);
            }
            for effect in session.tick(step * 200_000 + 1) {
                if let Effect::Telemetry(t) = effect {
                    for item in t.items {
                        assert!(
                            uxduck_core::domain::telemetry::key_says_unit(&item.key, item.unit),
                            "{} does not say {:?}",
                            item.key,
                            item.unit
                        );
                        keys.insert(item.key);
                    }
                }
            }
        }
        for key in [
            "battery.voltage_v",
            "speed.ground_mps",
            "heading.true_deg",
            "attitude.roll_deg",
        ] {
            assert!(keys.contains(key), "{key} not produced: {keys:?}");
        }
    }
}

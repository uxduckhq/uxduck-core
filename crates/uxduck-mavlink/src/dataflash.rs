//! ArduPilot's DataFlash logs (`.bin`, ADR-0039 R15), read with a reader of
//! UXDuck's own: `FMT` records describe every other record, so the file
//! says how to read itself. No outside parser, so the browser's engine
//! reads them unchanged.
//!
//! [`to_tlog`] is the producer for the map and instruments: it turns `GPS`,
//! `POS`, `ATT`, `BAT`, `VIBE`, `RCOU`, `ORGN`, `PARM`, `MODE`, `ARM`,
//! `EV` and `MSG` into the MAVLink messages the live session reads,
//! stamped on the log's clock, so a replay of a `.bin` is a replay of a
//! tlog, and collects its `CMD` records as the mission. The clock is GPS
//! time (week and milliseconds) where the log has a fix, else time since
//! boot.
//!
//! [`messages`] and [`series`] read every other record as it was logged,
//! at its own rate, for the Chart and the agent (ADR-0039 R15).

use std::collections::BTreeMap;

use mavio::dialects::ardupilotmega::enums::{
    GpsFixType, MavAutopilot, MavModeFlag, MavParamType, MavSeverity, MavState, MavType,
};
use mavio::dialects::ardupilotmega::messages::{
    Attitude, AutopilotVersion, BatteryStatus, GlobalPositionInt, GpsRawInt, Heartbeat,
    HomePosition, ParamValue, ServoOutputRaw, Statustext, SysStatus, SystemTime, VfrHud, Vibration,
};

use crate::decode::MissionItemInt;
use crate::sim::frame;

const HEAD: [u8; 2] = [0xA3, 0x95];
/// `FMT`'s own type and length.
const FMT: u8 = 128;
const FMT_LEN: usize = 89;
/// The GPS epoch, 1980-01-06, in Unix seconds, and the leap seconds since.
const GPS_EPOCH_S: u64 = 315_964_800;
const LEAP_S: u64 = 18;
/// `EV`'s armed and disarmed.
const EV_ARMED: u64 = 10;
const EV_DISARMED: u64 = 11;

/// Whether bytes look like a DataFlash log: its first record's header.
#[must_use]
pub fn is_dataflash(bytes: &[u8]) -> bool {
    bytes.starts_with(&HEAD) && bytes.get(2) == Some(&FMT)
}

/// A value of a record's column.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Text(String),
}

impl Value {
    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        #[allow(clippy::cast_precision_loss, reason = "log values")]
        match self {
            Self::Int(v) => Some(*v as f64),
            Self::Float(v) => Some(*v),
            Self::Text(_) => None,
        }
    }

    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(t) => Some(t),
            _ => None,
        }
    }
}

/// One message type as its `FMT` record describes it.
#[derive(Clone, Debug)]
pub struct Format {
    pub name: String,
    /// The whole record, with its three header bytes.
    pub len: usize,
    /// Each column's name, format character and offset in the payload.
    pub columns: Vec<(String, u8, usize)>,
}

/// How many bytes a format character takes; none for one not known.
fn size_of(c: u8) -> Option<usize> {
    Some(match c {
        b'b' | b'B' | b'M' => 1,
        b'h' | b'H' | b'c' | b'C' => 2,
        b'i' | b'I' | b'e' | b'E' | b'L' | b'f' | b'n' => 4,
        b'd' | b'q' | b'Q' => 8,
        b'N' => 16,
        b'Z' | b'a' => 64,
        _ => return None,
    })
}

fn text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

impl Format {
    fn parse(payload: &[u8]) -> Option<(u8, Self)> {
        let kind = *payload.first()?;
        let len = usize::from(*payload.get(1)?);
        let name = text(payload.get(2..6)?);
        let format = text(payload.get(6..22)?);
        let names = text(payload.get(22..86)?);
        let mut columns = Vec::new();
        let mut at = 0;
        for (c, column) in format.bytes().zip(names.split(',')) {
            columns.push((column.to_owned(), c, at));
            at += size_of(c)?;
        }
        (at + 3 <= len).then_some((kind, Self { name, len, columns }))
    }

    /// A column's value in a record's payload.
    #[must_use]
    #[allow(clippy::cast_precision_loss, reason = "a log's scaled integers")]
    pub fn value(&self, payload: &[u8], column: &str) -> Option<Value> {
        let &(_, c, at) = self.columns.iter().find(|(n, _, _)| n == column)?;
        let raw = payload.get(at..at + size_of(c)?)?;
        let le = |n: usize| -> [u8; 8] {
            let mut out = [0u8; 8];
            out[..n].copy_from_slice(&raw[..n]);
            out
        };
        let signed = |n: usize| -> i64 {
            let shift = 64 - 8 * u32::try_from(n).unwrap_or(8);
            (i64::from_le_bytes(le(n)) << shift) >> shift
        };
        let unsigned = |n: usize| u64::from_le_bytes(le(n));
        #[allow(clippy::cast_possible_wrap, reason = "a log's u64 counts")]
        Some(match c {
            b'b' => Value::Int(signed(1)),
            b'B' | b'M' => Value::Int(unsigned(1) as i64),
            b'h' => Value::Int(signed(2)),
            b'H' => Value::Int(unsigned(2) as i64),
            b'i' | b'L' => Value::Int(signed(4)),
            b'I' => Value::Int(unsigned(4) as i64),
            b'q' => Value::Int(signed(8)),
            b'Q' => Value::Int(unsigned(8) as i64),
            b'c' => Value::Float(signed(2) as f64 / 100.0),
            b'C' => Value::Float(unsigned(2) as f64 / 100.0),
            b'e' => Value::Float(signed(4) as f64 / 100.0),
            b'E' => Value::Float(unsigned(4) as f64 / 100.0),
            b'f' => Value::Float(f64::from(f32::from_le_bytes(raw[..4].try_into().ok()?))),
            b'd' => Value::Float(f64::from_le_bytes(raw[..8].try_into().ok()?)),
            b'n' | b'N' | b'Z' => Value::Text(text(raw)),
            _ => return None,
        })
    }
}

/// A log read: its formats and where each record is.
#[derive(Debug, Default)]
pub struct Log {
    pub formats: BTreeMap<u8, Format>,
    /// Each record's type and where its payload starts.
    pub records: Vec<(u8, usize)>,
    /// Bytes at the end that are not a whole record: a log cut short.
    pub cut: usize,
}

impl Log {
    /// The record's format and payload.
    #[must_use]
    pub fn record<'a>(&'a self, bytes: &'a [u8], index: usize) -> Option<(&'a Format, &'a [u8])> {
        let &(kind, at) = self.records.get(index)?;
        let format = self.formats.get(&kind)?;
        Some((format, bytes.get(at..at + format.len - 3)?))
    }
}

/// Reads a log: every whole record, skipping bytes that are not one (a
/// corrupt stretch) and stopping where it is cut short.
#[must_use]
pub fn read(bytes: &[u8]) -> Log {
    let mut log = Log::default();
    let mut at = 0;
    while at + 3 <= bytes.len() {
        if bytes[at..at + 2] != HEAD {
            at += 1;
            continue;
        }
        let kind = bytes[at + 2];
        let len = if kind == FMT {
            FMT_LEN
        } else if let Some(format) = log.formats.get(&kind) {
            format.len
        } else {
            at += 1;
            continue;
        };
        if at + len > bytes.len() {
            log.cut = bytes.len() - at;
            break;
        }
        let payload = &bytes[at + 3..at + len];
        if kind == FMT {
            if let Some((defined, format)) = Format::parse(payload) {
                log.formats.insert(defined, format);
            }
        } else {
            log.records.push((kind, at + 3));
        }
        at += len;
    }
    log
}

/// What a log said of its vehicle.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct About {
    /// `SYSID_THISMAV`, else 1.
    pub system_id: u8,
    /// Its firmware's line, such as "`ArduCopter V4.5.1 (abcdef12)`".
    pub firmware: Option<String>,
    /// Whether its times are GPS time; else time since boot.
    pub gps_time: bool,
    /// The mission its `CMD` records give, the last one logged; empty
    /// when it logged none.
    pub mission: Vec<MissionItemInt>,
    /// Each parameter's default, where its `PARM` records give one.
    pub defaults: BTreeMap<String, f32>,
}

/// The vehicle's type from its firmware's name.
fn vehicle_type(firmware: Option<&str>) -> MavType {
    let firmware = firmware.unwrap_or_default();
    if firmware.starts_with("ArduPlane") {
        MavType::FixedWing
    } else if firmware.starts_with("ArduRover") || firmware.starts_with("Rover") {
        MavType::GroundRover
    } else if firmware.starts_with("ArduSub") {
        MavType::Submarine
    } else if firmware.starts_with("Blimp") {
        MavType::Airship
    } else if firmware.starts_with("ArduCopter") {
        MavType::Quadrotor
    } else {
        MavType::Generic
    }
}

/// "V4.5.1" in a firmware line as `flight_sw_version`.
fn sw_version(firmware: &str) -> u32 {
    let Some(v) = firmware
        .split_whitespace()
        .find_map(|w| w.strip_prefix('V'))
    else {
        return 0;
    };
    let mut parts = v.split(['.', '-']).map(|p| p.parse::<u8>().unwrap_or(0));
    let (major, minor, patch) = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );
    u32::from_be_bytes([major, minor, patch, 255])
}

fn num(format: &Format, payload: &[u8], column: &str) -> Option<f64> {
    format.value(payload, column)?.as_f64()
}

/// The log's clock against Unix time in microseconds: its first 3D fix's
/// GPS week and milliseconds against that record's time since boot.
#[allow(clippy::cast_possible_truncation, reason = "a log's GPS week and ms")]
fn gps_offset(log: &Log, bytes: &[u8]) -> Option<i128> {
    (0..log.records.len()).find_map(|index| {
        let (format, payload) = log.record(bytes, index)?;
        if format.name != "GPS" {
            return None;
        }
        let week = num(format, payload, "GWk").unwrap_or(0.0);
        let ms = num(format, payload, "GMS").unwrap_or(0.0);
        let status = num(format, payload, "Status").unwrap_or(0.0);
        let boot = num(format, payload, "TimeUS").unwrap_or(0.0);
        (week > 0.0 && status >= 3.0).then(|| {
            (i128::from(GPS_EPOCH_S) + (week as i128) * 604_800 - i128::from(LEAP_S)) * 1_000_000
                + (ms as i128) * 1_000
                - boot as i128
        })
    })
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "a log's floats into MAVLink's"
)]
/// A `CMD` record as a mission item: `Lat` and `Lng` are degrees times
/// 1e7 already. A log older than `Frame` gives relative altitude.
fn mission_item(format: &Format, payload: &[u8]) -> Option<MissionItemInt> {
    let param = |c: &str| num(format, payload, c).unwrap_or(0.0) as f32;
    Some(MissionItemInt {
        seq: int(num(format, payload, "CNum")?),
        frame: num(format, payload, "Frame").map_or(3, int),
        command: int(num(format, payload, "CId")?),
        current: false,
        autocontinue: true,
        params: [param("Prm1"), param("Prm2"), param("Prm3"), param("Prm4")],
        x: int(num(format, payload, "Lat").unwrap_or(0.0)),
        y: int(num(format, payload, "Lng").unwrap_or(0.0)),
        z: param("Alt"),
        mission_type: 0,
    })
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    reason = "a log's values into MAVLink's fields"
)]
fn int<T: TryFrom<i64> + Default>(v: f64) -> T {
    T::try_from(v.round() as i64).unwrap_or_default()
}

/// The log as tlog records on its own clock (R15), and what it said of its
/// vehicle.
///
/// # Errors
/// When it holds no records UXDuck can play.
#[allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "one arm per kind of record"
)]
pub fn to_tlog(bytes: &[u8]) -> Result<(Vec<u8>, About), String> {
    let log = read(bytes);
    if log.records.is_empty() {
        return Err("This log holds no records UXDuck can read.".into());
    }
    // What it said of itself, and its clock: the first GPS fix's time.
    let mut about = About {
        system_id: 1,
        ..About::default()
    };
    let offset = gps_offset(&log, bytes);
    // Its parameters, each with its place in the list as first logged.
    let mut parameters: BTreeMap<String, u16> = BTreeMap::new();
    let mut mission: BTreeMap<u16, MissionItemInt> = BTreeMap::new();
    let mut mission_total: Option<u16> = None;
    for index in 0..log.records.len() {
        let Some((format, payload)) = log.record(bytes, index) else {
            continue;
        };
        match format.name.as_str() {
            "PARM"
                if format
                    .value(payload, "Name")
                    .and_then(|v| v.as_text().map(str::to_owned))
                    == Some("SYSID_THISMAV".into()) =>
            {
                if let Some(id) = num(format, payload, "Value") {
                    about.system_id = int::<u8>(id).max(1);
                }
            }
            "MSG" if about.firmware.is_none() => {
                if let Some(text) = format
                    .value(payload, "Message")
                    .and_then(|v| v.as_text().map(str::to_owned))
                    && ["Ardu", "Rover", "Blimp"]
                        .iter()
                        .any(|p| text.starts_with(p))
                    && text.contains(" V")
                {
                    about.firmware = Some(text);
                }
            }
            "CMD" => {
                if let Some(item) = mission_item(format, payload) {
                    mission_total = num(format, payload, "CTot").map(int::<u16>);
                    mission.insert(item.seq, item);
                }
            }
            _ => {}
        }
        if format.name == "PARM"
            && let Some(name) = format
                .value(payload, "Name")
                .and_then(|v| v.as_text().map(str::to_owned))
        {
            if let Some(default) = num(format, payload, "Default") {
                about.defaults.insert(name.clone(), default as f32);
            }
            let next = u16::try_from(parameters.len()).unwrap_or(u16::MAX);
            parameters.entry(name).or_insert(next);
        }
    }
    about.mission = mission
        .into_values()
        .filter(|item| mission_total.is_none_or(|total| item.seq < total))
        .collect();
    about.gps_time = offset.is_some();
    let offset = offset.unwrap_or(0);
    let kind = vehicle_type(about.firmware.as_deref());
    let system = about.system_id;
    let mut out = Vec::new();
    let mut sequence = 0u8;
    let push = |out: &mut Vec<u8>, stamp: u64, frame_bytes: Vec<u8>| {
        let _ = crate::tlog::record(out, stamp, &frame_bytes);
    };
    let mut next = || {
        sequence = sequence.wrapping_add(1);
        sequence
    };
    // Its first mode from the start, rather than mode 0 until it is logged.
    let first_mode = (0..log.records.len()).find_map(|index| {
        let (format, payload) = log.record(bytes, index)?;
        (format.name == "MODE").then(|| num(format, payload, "ModeNum"))?
    });
    let (mut armed, mut mode) = (false, first_mode.map_or(0, int::<u32>));
    let mut sent: BTreeMap<&str, u64> = BTreeMap::new();
    let mut heartbeat_at: Option<u64> = None;
    let mut version_sent = false;
    let has_pos = log
        .records
        .iter()
        .any(|(k, _)| log.formats.get(k).is_some_and(|f| f.name == "POS"));
    // Not more often than this, in microseconds of the log's time.
    let every = |name: &str| -> u64 {
        match name {
            "POS" | "ATT" | "RCOU" => 100_000,
            "GPS" => 200_000,
            "VIBE" => 500_000,
            _ => 1_000_000,
        }
    };
    let parameter_count = u16::try_from(parameters.len()).unwrap_or(u16::MAX);
    let mut battery_at: BTreeMap<u8, u64> = BTreeMap::new();
    let mut home: Option<(i32, i32, i32)> = None;
    for index in 0..log.records.len() {
        let Some((format, payload)) = log.record(bytes, index) else {
            continue;
        };
        let Some(boot) = num(format, payload, "TimeUS") else {
            continue;
        };
        let stamp_i = boot as i128 + offset;
        if stamp_i < 0 {
            continue;
        }
        let stamp = stamp_i as u64;
        let name = format.name.as_str();
        let due = |sent: &BTreeMap<&str, u64>| {
            sent.get(name)
                .is_none_or(|at| stamp.saturating_sub(*at) >= every(name))
        };
        let mut changed = false;
        match name {
            "MODE" => {
                if let Some(n) = num(format, payload, "ModeNum") {
                    mode = int::<u32>(n);
                    changed = true;
                }
            }
            "ARM" => {
                if let Some(state) = num(format, payload, "ArmState") {
                    armed = state > 0.0;
                    changed = true;
                }
            }
            "EV" => match num(format, payload, "Id").map(|v| v as u64) {
                Some(EV_ARMED) => {
                    armed = true;
                    changed = true;
                }
                Some(EV_DISARMED) => {
                    armed = false;
                    changed = true;
                }
                _ => {}
            },
            _ => {}
        }
        // A heartbeat each second, and at once when arming or the mode changes.
        if changed || heartbeat_at.is_none_or(|at| stamp.saturating_sub(at) >= 1_000_000) {
            heartbeat_at = Some(stamp);
            let mut base_mode = MavModeFlag::CUSTOM_MODE_ENABLED;
            if armed {
                base_mode |= MavModeFlag::SAFETY_ARMED;
            }
            let heartbeat = Heartbeat {
                type_: kind,
                autopilot: MavAutopilot::Ardupilotmega,
                base_mode,
                custom_mode: mode,
                system_status: if armed {
                    MavState::Active
                } else {
                    MavState::Standby
                },
                mavlink_version: 3,
            };
            push(&mut out, stamp, frame(system, 1, next(), &heartbeat));
            // GPS time, so the replay's clock matches it to other sources.
            if about.gps_time {
                let time = SystemTime {
                    time_unix_usec: stamp,
                    time_boot_ms: (boot / 1000.0) as u32,
                };
                push(&mut out, stamp, frame(system, 1, next(), &time));
            }
            if !version_sent {
                version_sent = true;
                let version = AutopilotVersion {
                    flight_sw_version: about.firmware.as_deref().map_or(0, sw_version),
                    ..AutopilotVersion::default()
                };
                push(&mut out, stamp, frame(system, 1, next(), &version));
            }
        }
        let time_boot_ms = (boot / 1000.0) as u32;
        match name {
            "POS" if due(&sent) => {
                let (Some(lat), Some(lng)) =
                    (num(format, payload, "Lat"), num(format, payload, "Lng"))
                else {
                    continue;
                };
                let position = GlobalPositionInt {
                    time_boot_ms,
                    lat: int(lat),
                    lon: int(lng),
                    alt: int(num(format, payload, "Alt").unwrap_or(0.0) * 1000.0),
                    relative_alt: int(num(format, payload, "RelHomeAlt").unwrap_or(0.0) * 1000.0),
                    hdg: u16::MAX,
                    ..GlobalPositionInt::default()
                };
                push(&mut out, stamp, frame(system, 1, next(), &position));
                sent.insert("POS", stamp);
            }
            "GPS" if due(&sent) && num(format, payload, "I").unwrap_or(0.0) == 0.0 => {
                let (Some(lat), Some(lng)) =
                    (num(format, payload, "Lat"), num(format, payload, "Lng"))
                else {
                    continue;
                };
                let alt = num(format, payload, "Alt").unwrap_or(0.0);
                let speed = num(format, payload, "Spd").unwrap_or(0.0);
                let course = num(format, payload, "GCrs").unwrap_or(0.0);
                let raw = GpsRawInt {
                    time_usec: stamp,
                    fix_type: match num(format, payload, "Status").unwrap_or(0.0) as u8 {
                        0 => GpsFixType::NoGps,
                        1 => GpsFixType::NoFix,
                        2 => GpsFixType::_2dFix,
                        4 => GpsFixType::Dgps,
                        5 => GpsFixType::RtkFloat,
                        6 => GpsFixType::RtkFixed,
                        _ => GpsFixType::_3dFix,
                    },
                    lat: int(lat),
                    lon: int(lng),
                    alt: int(alt * 1000.0),
                    eph: int(num(format, payload, "HDop").unwrap_or(0.0) * 100.0),
                    epv: u16::MAX,
                    vel: int(speed * 100.0),
                    cog: int(course * 100.0),
                    satellites_visible: int(num(format, payload, "NSats").unwrap_or(0.0)),
                    ..GpsRawInt::default()
                };
                push(&mut out, stamp, frame(system, 1, next(), &raw));
                let hud = VfrHud {
                    groundspeed: speed as f32,
                    alt: alt as f32,
                    climb: -(num(format, payload, "VZ").unwrap_or(0.0) as f32),
                    heading: int(course),
                    ..VfrHud::default()
                };
                push(&mut out, stamp, frame(system, 1, next(), &hud));
                if !has_pos {
                    let position = GlobalPositionInt {
                        time_boot_ms,
                        lat: int(lat),
                        lon: int(lng),
                        alt: int(alt * 1000.0),
                        hdg: u16::MAX,
                        ..GlobalPositionInt::default()
                    };
                    push(&mut out, stamp, frame(system, 1, next(), &position));
                }
                sent.insert("GPS", stamp);
            }
            "ATT" if due(&sent) => {
                let radians =
                    |c: &str| (num(format, payload, c).unwrap_or(0.0).to_radians()) as f32;
                let attitude = Attitude {
                    time_boot_ms,
                    roll: radians("Roll"),
                    pitch: radians("Pitch"),
                    yaw: radians("Yaw"),
                    ..Attitude::default()
                };
                push(&mut out, stamp, frame(system, 1, next(), &attitude));
                sent.insert("ATT", stamp);
            }
            "BAT" => {
                let instance = num(format, payload, "Inst").map_or(0, int::<u8>);
                if battery_at
                    .get(&instance)
                    .is_some_and(|at| stamp.saturating_sub(*at) < every("BAT"))
                {
                    continue;
                }
                battery_at.insert(instance, stamp);
                let volts = num(format, payload, "Volt").unwrap_or(0.0);
                let current = num(format, payload, "Curr").unwrap_or(-0.01);
                let remaining = num(format, payload, "RemPct").map_or(-1, int);
                if instance == 0 {
                    let status = SysStatus {
                        voltage_battery: int(volts * 1000.0),
                        current_battery: int(current * 100.0),
                        battery_remaining: remaining,
                        ..SysStatus::default()
                    };
                    push(&mut out, stamp, frame(system, 1, next(), &status));
                }
                let mut voltages = [u16::MAX; 10];
                voltages[0] = int(volts * 1000.0);
                let battery = BatteryStatus {
                    id: instance,
                    temperature: num(format, payload, "Temp").map_or(i16::MAX, |c| int(c * 100.0)),
                    voltages,
                    current_battery: int(current * 100.0),
                    current_consumed: num(format, payload, "CurrTot").map_or(-1, int),
                    energy_consumed: num(format, payload, "EnrgTot")
                        .map_or(-1, |wh| int(wh * 360.0)),
                    battery_remaining: remaining,
                    ..BatteryStatus::default()
                };
                push(&mut out, stamp, frame(system, 1, next(), &battery));
            }
            "RCOU" if due(&sent) => {
                let pulse = |n: usize| num(format, payload, &format!("C{n}")).map_or(0, int::<u16>);
                let servos = ServoOutputRaw {
                    time_usec: boot as u32,
                    port: 0,
                    servo1_raw: pulse(1),
                    servo2_raw: pulse(2),
                    servo3_raw: pulse(3),
                    servo4_raw: pulse(4),
                    servo5_raw: pulse(5),
                    servo6_raw: pulse(6),
                    servo7_raw: pulse(7),
                    servo8_raw: pulse(8),
                    servo9_raw: pulse(9),
                    servo10_raw: pulse(10),
                    servo11_raw: pulse(11),
                    servo12_raw: pulse(12),
                    servo13_raw: pulse(13),
                    servo14_raw: pulse(14),
                    servo15_raw: pulse(15),
                    servo16_raw: pulse(16),
                };
                push(&mut out, stamp, frame(system, 1, next(), &servos));
                sent.insert("RCOU", stamp);
            }
            // The home ArduPilot set (type 1; 0 is the estimator's origin).
            "ORGN" if num(format, payload, "Type") == Some(1.0) => {
                let place = (
                    num(format, payload, "Lat").map_or(0, int::<i32>),
                    num(format, payload, "Lng").map_or(0, int::<i32>),
                    num(format, payload, "Alt").map_or(0, |m| int::<i32>(m * 1000.0)),
                );
                if home != Some(place) {
                    home = Some(place);
                    let position = HomePosition {
                        latitude: place.0,
                        longitude: place.1,
                        altitude: place.2,
                        q: [1.0, 0.0, 0.0, 0.0],
                        time_usec: stamp,
                        ..HomePosition::default()
                    };
                    push(&mut out, stamp, frame(system, 1, next(), &position));
                }
            }
            "PARM" => {
                let (Some(name), Some(value)) = (
                    format
                        .value(payload, "Name")
                        .and_then(|v| v.as_text().map(str::to_owned)),
                    num(format, payload, "Value"),
                ) else {
                    continue;
                };
                let mut param_id = [0u8; 16];
                let len = name.len().min(16);
                param_id[..len].copy_from_slice(&name.as_bytes()[..len]);
                let param = ParamValue {
                    param_id,
                    param_value: value as f32,
                    param_type: MavParamType::Real32,
                    param_count: parameter_count,
                    param_index: parameters.get(&name).copied().unwrap_or(u16::MAX),
                };
                push(&mut out, stamp, frame(system, 1, next(), &param));
            }
            "VIBE" if due(&sent) && num(format, payload, "IMU").unwrap_or(0.0) == 0.0 => {
                let vibration = Vibration {
                    time_usec: stamp,
                    vibration_x: num(format, payload, "VibeX").unwrap_or(0.0) as f32,
                    vibration_y: num(format, payload, "VibeY").unwrap_or(0.0) as f32,
                    vibration_z: num(format, payload, "VibeZ").unwrap_or(0.0) as f32,
                    clipping_0: int(num(format, payload, "Clip").unwrap_or(0.0)),
                    ..Vibration::default()
                };
                push(&mut out, stamp, frame(system, 1, next(), &vibration));
                sent.insert("VIBE", stamp);
            }
            "MSG" => {
                if let Some(message) = format
                    .value(payload, "Message")
                    .and_then(|v| v.as_text().map(str::to_owned))
                {
                    let mut text = [0u8; 50];
                    let len = message.len().min(50);
                    text[..len].copy_from_slice(&message.as_bytes()[..len]);
                    let status = Statustext {
                        severity: MavSeverity::Info,
                        text,
                        ..Statustext::default()
                    };
                    push(&mut out, stamp, frame(system, 1, next(), &status));
                }
            }
            _ => {}
        }
    }
    if out.is_empty() {
        return Err("This log holds no records UXDuck can read.".into());
    }
    Ok((out, about))
}

/// Records that describe the log rather than the vehicle.
const ABOUT_THE_LOG: [&str; 4] = ["FMTU", "UNIT", "MULT", "FILE"];

/// One column of a message as logged.
#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    pub name: String,
    /// Its unit as the log names it (`UNIT`), such as "m/s" or "deg".
    pub unit: Option<String>,
}

/// One message type as logged: its columns with numbers, its instances
/// (`VIBE[0]`, `VIBE[1]`) and how often it was written.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub name: String,
    pub columns: Vec<Column>,
    /// The instance column's values; empty for a message of one instance.
    pub instances: Vec<i64>,
    pub records: usize,
    /// Records a second, over the time it was written.
    pub rate_hz: f64,
}

/// What each column of a format means: its unit, its multiplier and
/// whether it is the instance, from `FMTU`, `UNIT` and `MULT`.
#[derive(Debug, Default)]
struct Meaning {
    units: Vec<Option<String>>,
    mults: Vec<Option<f64>>,
    instance: Option<usize>,
}

fn meanings(log: &Log, bytes: &[u8]) -> BTreeMap<u8, Meaning> {
    let mut labels: BTreeMap<i64, String> = BTreeMap::new();
    let mut mults: BTreeMap<i64, f64> = BTreeMap::new();
    let mut ids: Vec<(u8, String, String)> = Vec::new();
    for index in 0..log.records.len() {
        let Some((format, payload)) = log.record(bytes, index) else {
            continue;
        };
        let id = || {
            format
                .value(payload, "Id")
                .and_then(|v| v.as_f64())
                .map(int::<i64>)
        };
        match format.name.as_str() {
            "UNIT" => {
                if let (Some(id), Some(Value::Text(label))) = (id(), format.value(payload, "Label"))
                {
                    labels.insert(id, label);
                }
            }
            "MULT" => {
                if let (Some(id), Some(mult)) = (id(), num(format, payload, "Mult")) {
                    mults.insert(id, mult);
                }
            }
            "FMTU" => {
                let text = |c: &str| {
                    format
                        .value(payload, c)
                        .and_then(|v| v.as_text().map(str::to_owned))
                        .unwrap_or_default()
                };
                if let Some(kind) = num(format, payload, "FmtType") {
                    ids.push((int(kind), text("UnitIds"), text("MultIds")));
                }
            }
            _ => {}
        }
    }
    ids.into_iter()
        .map(|(kind, units, multipliers)| {
            let units: Vec<Option<String>> = units
                .bytes()
                .map(|c| labels.get(&i64::from(c)).cloned())
                .collect();
            let instance = units.iter().position(|u| u.as_deref() == Some("#"));
            let mults = multipliers
                .bytes()
                .map(|c| mults.get(&i64::from(c)).copied())
                .collect();
            (
                kind,
                Meaning {
                    units,
                    mults,
                    instance,
                },
            )
        })
        .collect()
}

/// A column's number as its unit says: `L` in degrees, and a plain
/// integer times its multiplier (an altitude logged in centimetres in
/// metres). `c`, `C`, `e` and `E` are scaled by the reader already.
fn scaled(format: &Format, payload: &[u8], column: usize, mult: Option<f64>) -> Option<f64> {
    let (name, c, _) = format.columns.get(column)?;
    let value = num(format, payload, name)?;
    Some(match c {
        b'L' => value * 1e-7,
        b'b' | b'B' | b'h' | b'H' | b'i' | b'I' | b'q' | b'Q' => match mult {
            Some(m) if m.is_finite() && m != 0.0 => value * m,
            _ => value,
        },
        _ => value,
    })
}

fn numeric(c: u8) -> bool {
    !matches!(c, b'n' | b'N' | b'Z' | b'a')
}

/// Every message the log holds with numbers in it, as logged (R15): what
/// the Chart offers and `replay/logFields` lists.
#[must_use]
#[allow(clippy::cast_precision_loss, reason = "record counts")]
pub fn messages(bytes: &[u8]) -> Vec<Message> {
    let log = read(bytes);
    let meanings = meanings(&log, bytes);
    let mut seen: BTreeMap<u8, (usize, f64, f64, std::collections::BTreeSet<i64>)> =
        BTreeMap::new();
    for index in 0..log.records.len() {
        let Some((format, payload)) = log.record(bytes, index) else {
            continue;
        };
        let kind = log.records[index].0;
        let time = num(format, payload, "TimeUS").unwrap_or(0.0);
        let entry = seen
            .entry(kind)
            .or_insert((0, time, time, std::collections::BTreeSet::new()));
        entry.0 += 1;
        entry.1 = entry.1.min(time);
        entry.2 = entry.2.max(time);
        if let Some(column) = meanings.get(&kind).and_then(|m| m.instance)
            && let Some(name) = format.columns.get(column).map(|c| c.0.as_str())
            && let Some(instance) = num(format, payload, name)
        {
            entry.3.insert(int::<i64>(instance));
        }
    }
    let mut out: Vec<Message> = seen
        .into_iter()
        .filter_map(|(kind, (records, first, last, instances))| {
            let format = log.formats.get(&kind)?;
            if ABOUT_THE_LOG.contains(&format.name.as_str()) {
                return None;
            }
            let meaning = meanings.get(&kind);
            let columns: Vec<Column> = format
                .columns
                .iter()
                .enumerate()
                .filter(|(i, (name, c, _))| {
                    name != "TimeUS" && numeric(*c) && meaning.and_then(|m| m.instance) != Some(*i)
                })
                .map(|(i, (name, _, _))| Column {
                    name: name.clone(),
                    unit: meaning
                        .and_then(|m| m.units.get(i).cloned().flatten())
                        .filter(|u| !u.is_empty() && u != "#"),
                })
                .collect();
            if columns.is_empty() {
                return None;
            }
            let span_s = (last - first) / 1e6;
            Some(Message {
                name: format.name.clone(),
                columns,
                instances: if meaning.and_then(|m| m.instance).is_some() {
                    instances.into_iter().collect()
                } else {
                    Vec::new()
                },
                records,
                rate_hz: if span_s > 0.0 {
                    (records.saturating_sub(1)) as f64 / span_s
                } else {
                    0.0
                },
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// A key for [`series`]: `df.<MESSAGE>.<Column>`, or
/// `df.<MESSAGE>[<instance>].<Column>` for a message of several instances
/// (its first instance when none is given).
#[must_use]
pub fn parse_key(key: &str) -> Option<(&str, Option<i64>, &str)> {
    let rest = key.strip_prefix("df.")?;
    let (message, column) = rest.split_once('.')?;
    match message.split_once('[') {
        Some((name, instance)) => {
            let instance = instance.strip_suffix(']')?.parse().ok()?;
            Some((name, Some(instance), column))
        }
        None => Some((message, None, column)),
    }
}

/// A key, its format, its column and, for a message of several
/// instances, the instance column and the instance wanted.
type Wanted<'a> = (&'a String, u8, usize, Option<(usize, i64)>);

/// Each key's values as logged, on the log's clock (the same stamps
/// [`to_tlog`] gives), in the order written. A key the log does not hold
/// is left out.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a log's times on its clock"
)]
pub fn series(bytes: &[u8], keys: &[String]) -> BTreeMap<String, Vec<(u64, f64)>> {
    let log = read(bytes);
    let meanings = meanings(&log, bytes);
    let offset = gps_offset(&log, bytes).unwrap_or(0);
    // Each key's format, column and instance, the first instance when the
    // key names none.
    let mut wanted: Vec<Wanted> = Vec::new();
    for key in keys {
        let Some((name, instance, column)) = parse_key(key) else {
            continue;
        };
        let Some((&kind, format)) = log.formats.iter().find(|(_, f)| f.name == name) else {
            continue;
        };
        let Some(at) = format.columns.iter().position(|c| c.0 == column) else {
            continue;
        };
        let which = meanings.get(&kind).and_then(|m| m.instance).map(|i| {
            let first = || {
                (0..log.records.len())
                    .filter(|r| log.records[*r].0 == kind)
                    .find_map(|r| {
                        let (format, payload) = log.record(bytes, r)?;
                        num(format, payload, &format.columns[i].0).map(|v| v as i64)
                    })
                    .unwrap_or(0)
            };
            (i, instance.unwrap_or_else(first))
        });
        wanted.push((key, kind, at, which));
    }
    let mut out: BTreeMap<String, Vec<(u64, f64)>> = BTreeMap::new();
    for index in 0..log.records.len() {
        let kind = log.records[index].0;
        if !wanted.iter().any(|w| w.1 == kind) {
            continue;
        }
        let Some((format, payload)) = log.record(bytes, index) else {
            continue;
        };
        let Some(boot) = num(format, payload, "TimeUS") else {
            continue;
        };
        let stamp = boot as i128 + offset;
        if stamp < 0 {
            continue;
        }
        let mult = |at: usize| {
            meanings
                .get(&kind)
                .and_then(|m| m.mults.get(at).copied().flatten())
        };
        for (key, _, at, which) in wanted.iter().filter(|w| w.1 == kind) {
            if let Some((column, instance)) = which
                && num(format, payload, &format.columns[*column].0).map(|v| v as i64)
                    != Some(*instance)
            {
                continue;
            }
            if let Some(value) = scaled(format, payload, *at, mult(*at)).filter(|v| v.is_finite()) {
                out.entry((*key).clone())
                    .or_default()
                    .push((stamp as u64, value));
            }
        }
    }
    out
}

/// DataFlash logs made up for tests here and in the engine, as the fake
/// vehicle is for tlogs.
pub mod fixture {
    use super::{FMT, Value, size_of};
    use std::collections::BTreeMap;

    /// A DataFlash writer for tests: formats first, then records.
    #[derive(Debug, Default)]
    pub struct Writer {
        pub bytes: Vec<u8>,
        formats: BTreeMap<String, (u8, String)>,
    }

    impl Writer {
        pub fn format(&mut self, kind: u8, name: &str, format: &str, columns: &str) {
            let len: usize = 3 + format
                .bytes()
                .map(|c| size_of(c).unwrap_or(0))
                .sum::<usize>();
            let mut record = vec![0xA3, 0x95, FMT, kind, u8::try_from(len).unwrap_or(0)];
            let mut field = |text: &str, n: usize| {
                let mut f = vec![0u8; n];
                f[..text.len().min(n)].copy_from_slice(&text.as_bytes()[..text.len().min(n)]);
                record.extend_from_slice(&f);
            };
            field(name, 4);
            field(format, 16);
            field(columns, 64);
            self.bytes.extend_from_slice(&record);
            self.formats
                .insert(name.to_owned(), (kind, format.to_owned()));
        }

        /// A record; numbers in column order, text where the format has it.
        pub fn record(&mut self, name: &str, values: &[Value]) {
            let (kind, format) = self.formats[name].clone();
            self.bytes.extend_from_slice(&[0xA3, 0x95, kind]);
            for (c, value) in format.bytes().zip(values) {
                let n = value.as_f64().unwrap_or(0.0);
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "test values"
                )]
                match c {
                    b'b' | b'B' | b'M' => self.bytes.push(n as u8),
                    b'h' | b'H' => self.bytes.extend_from_slice(&(n as i16).to_le_bytes()),
                    b'c' => self
                        .bytes
                        .extend_from_slice(&((n * 100.0) as i16).to_le_bytes()),
                    b'i' | b'I' | b'L' => self.bytes.extend_from_slice(&(n as i32).to_le_bytes()),
                    b'e' => self
                        .bytes
                        .extend_from_slice(&((n * 100.0) as i32).to_le_bytes()),
                    b'f' => self.bytes.extend_from_slice(&(n as f32).to_le_bytes()),
                    b'd' => self.bytes.extend_from_slice(&n.to_le_bytes()),
                    b'q' | b'Q' => self.bytes.extend_from_slice(&(n as i64).to_le_bytes()),
                    b'N' | b'Z' | b'n' => {
                        let size = size_of(c).unwrap_or(0);
                        let mut f = vec![0u8; size];
                        let t = value.as_text().unwrap_or_default().as_bytes();
                        f[..t.len().min(size)].copy_from_slice(&t[..t.len().min(size)]);
                        self.bytes.extend_from_slice(&f);
                    }
                    _ => {}
                }
            }
        }
    }

    /// A copter's minute: GPS week 2389, armed at 10 s, GUIDED then RTL,
    /// up to 20 m and back, disarmed at 50 s; two batteries, two IMUs, its
    /// motors, home, a three-item mission and units for every column.
    #[must_use]
    #[allow(
        clippy::too_many_lines,
        clippy::cast_precision_loss,
        reason = "a made-up minute, record by record"
    )]
    pub fn flight() -> Vec<u8> {
        let mut w = Writer::default();
        w.format(60, "MSG", "QBBZ", "TimeUS,ID,Seq,Message");
        w.format(32, "PARM", "QNff", "TimeUS,Name,Value,Default");
        w.format(
            95,
            "GPS",
            "QBBIHBcLLeffffB",
            "TimeUS,I,Status,GMS,GWk,NSats,HDop,Lat,Lng,Alt,Spd,GCrs,VZ,Yaw,U",
        );
        w.format(
            75,
            "POS",
            "QLLfff",
            "TimeUS,Lat,Lng,Alt,RelHomeAlt,RelOriginAlt",
        );
        w.format(
            73,
            "ATT",
            "QffffffB",
            "TimeUS,DesRoll,Roll,DesPitch,Pitch,DesYaw,Yaw,AEKF",
        );
        w.format(123, "MODE", "QMBB", "TimeUS,Mode,ModeNum,Rsn");
        w.format(184, "EV", "QB", "TimeUS,Id");
        w.format(169, "VIBE", "QBfffI", "TimeUS,IMU,VibeX,VibeY,VibeZ,Clip");
        w.format(
            147,
            "BAT",
            "QBfffffcf",
            "TimeUS,Inst,Volt,VoltR,Curr,CurrTot,EnrgTot,Temp,RemPct",
        );
        w.format(
            67,
            "RCOU",
            "QHHHHHHHHHHHHHH",
            "TimeUS,C1,C2,C3,C4,C5,C6,C7,C8,C9,C10,C11,C12,C13,C14",
        );
        w.format(80, "ORGN", "QBLLe", "TimeUS,Type,Lat,Lng,Alt");
        w.format(
            90,
            "CMD",
            "QHHHffffLLfB",
            "TimeUS,CTot,CNum,CId,Prm1,Prm2,Prm3,Prm4,Lat,Lng,Alt,Frame",
        );
        w.format(177, "UNIT", "QbZ", "TimeUS,Id,Label");
        w.format(178, "MULT", "Qbd", "TimeUS,Id,Mult");
        w.format(179, "FMTU", "QBNN", "TimeUS,FmtType,UnitIds,MultIds");
        let i = |v: i64| Value::Int(v);
        let f = Value::Float;
        let t = |s: &str| Value::Text(s.into());
        w.record(
            "MSG",
            &[i(1_000_000), i(0), i(0), t("ArduCopter V4.5.1 (abcdef12)")],
        );
        for (id, label) in [
            ('s', "s"),
            ('#', "#"),
            ('m', "m"),
            ('n', "m/s"),
            ('d', "deg"),
            ('D', "deglatitude"),
            ('U', "deglongitude"),
            ('v', "V"),
            ('A', "A"),
            ('a', "Ah"),
            ('O', "degC"),
            ('%', "%"),
            ('Y', "us"),
            ('-', ""),
        ] {
            w.record("UNIT", &[i(0), i(id as i64), t(label)]);
        }
        for (id, mult) in [
            ('-', 0.0),
            ('0', 1.0),
            ('B', 1e-2),
            ('F', 1e-6),
            ('G', 1e-7),
        ] {
            w.record("MULT", &[i(0), i(id as i64), f(mult)]);
        }
        w.record("FMTU", &[i(0), i(169), t("s#----"), t("F-----")]);
        w.record("FMTU", &[i(0), i(147), t("s#vvAa-O%"), t("F000000B0")]);
        w.record("FMTU", &[i(0), i(73), t("sdddddd-"), t("F000000-")]);
        w.record(
            "FMTU",
            &[i(0), i(67), t("sYYYYYYYYYYYYYY"), t("F00000000000000")],
        );
        w.record(
            "FMTU",
            &[i(0), i(95), t("s#-s---DUmndnd-"), t("F--C---GGB000--")],
        );
        w.record("PARM", &[i(1_100_000), t("SYSID_THISMAV"), f(7.0), f(1.0)]);
        w.record("PARM", &[i(1_100_000), t("RTL_ALT"), f(2500.0), f(1500.0)]);
        w.record("MODE", &[i(1_200_000), i(4), i(4), i(0)]);
        // A takeoff to 20 m, a waypoint, then home: the home item first.
        for (seq, id, lat, alt) in [
            (0, 16, 515_000_000, 0.0),
            (1, 22, 0, 20.0),
            (2, 16, 515_000_300, 20.0),
        ] {
            w.record(
                "CMD",
                &[
                    i(1_300_000),
                    i(3),
                    i(seq),
                    i(id),
                    f(0.0),
                    f(0.0),
                    f(0.0),
                    f(0.0),
                    i(lat),
                    i(if lat == 0 { 0 } else { -1_000_000 }),
                    f(alt),
                    i(3),
                ],
            );
        }
        for tenth in 20..=600i64 {
            let us = tenth * 100_000;
            let secs = us as f64 / 1e6;
            let height = if (12.0..48.0).contains(&secs) {
                20.0 * (1.0 - ((secs - 30.0) / 18.0).abs())
            } else {
                0.0
            };
            if tenth % 2 == 0 {
                // GPS week 2389, 100 000 s into it.
                w.record(
                    "GPS",
                    &[
                        i(us),
                        i(0),
                        i(3),
                        i(100_000_000 + us / 1000),
                        i(2389),
                        i(12),
                        f(0.8),
                        i(515_000_000 + tenth),
                        i(-1_000_000),
                        f(100.0 + height),
                        f(2.0),
                        f(90.0),
                        f(0.0),
                        f(0.0),
                        i(1),
                    ],
                );
            }
            w.record(
                "POS",
                &[
                    i(us),
                    i(515_000_000 + tenth),
                    i(-1_000_000),
                    f(100.0 + height),
                    f(height),
                    f(height),
                ],
            );
            w.record(
                "ATT",
                &[
                    i(us),
                    f(0.0),
                    f(5.0),
                    f(0.0),
                    f(-3.0),
                    f(0.0),
                    f(90.0),
                    i(1),
                ],
            );
            if tenth % 5 == 0 {
                w.record("VIBE", &[i(us), i(0), f(12.0), f(14.0), f(30.0), i(0)]);
                w.record("VIBE", &[i(us), i(1), f(11.0), f(13.0), f(45.0), i(2)]);
            }
            let flying = (10.0..50.0).contains(&secs);
            let motor = if flying { 1500 } else { 1000 };
            w.record(
                "RCOU",
                &[
                    i(us),
                    i(motor),
                    i(motor + 10),
                    i(motor + 20),
                    i(motor + 30),
                    i(0),
                    i(0),
                    i(0),
                    i(0),
                    i(0),
                    i(0),
                    i(0),
                    i(0),
                    i(0),
                    i(0),
                ],
            );
            if tenth % 10 == 0 {
                let used = secs * 2.0;
                for (inst, volts) in [(0, 16.4 - secs * 0.01), (1, 12.2)] {
                    w.record(
                        "BAT",
                        &[
                            i(us),
                            i(inst),
                            f(volts),
                            f(volts),
                            f(if flying { 12.0 } else { 0.5 }),
                            f(used),
                            f(used / 100.0),
                            f(31.5),
                            f(90.0),
                        ],
                    );
                }
            }
            if tenth == 95 {
                w.record(
                    "ORGN",
                    &[i(us), i(1), i(515_000_000), i(-1_000_000), f(100.0)],
                );
            }
            if tenth == 100 {
                w.record("EV", &[i(us), i(10)]);
            }
            if tenth == 250 {
                w.record("MODE", &[i(us), i(6), i(6), i(0)]);
            }
            if tenth == 500 {
                w.record("EV", &[i(us), i(11)]);
            }
        }
        w.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{Writer, flight};
    use super::*;

    #[test]
    fn a_log_reads_itself_from_its_formats() {
        let bytes = flight();
        assert!(is_dataflash(&bytes));
        let log = read(&bytes);
        assert_eq!(log.cut, 0);
        let names: Vec<&str> = log.formats.values().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"GPS") && names.contains(&"VIBE"));
        let (format, payload) = log.record(&bytes, 0).unwrap();
        assert_eq!(format.name, "MSG");
        assert_eq!(
            format.value(payload, "Message"),
            Some(Value::Text("ArduCopter V4.5.1 (abcdef12)".into()))
        );
    }

    #[test]
    fn a_log_cut_short_plays_to_where_it_ends() {
        let bytes = flight();
        let cut = &bytes[..bytes.len() / 2 + 7];
        let log = read(cut);
        assert!(log.cut > 0);
        let (tlog, _) = to_tlog(cut).unwrap();
        let whole = crate::tlog::read(&to_tlog(&bytes).unwrap().0).len();
        let half = crate::tlog::read(&tlog).len();
        assert!(half > whole / 3 && half < whole, "{half} of {whole}");
    }

    #[test]
    fn the_producer_gives_the_live_sessions_messages_on_gps_time() {
        let bytes = flight();
        let (tlog, about) = to_tlog(&bytes).unwrap();
        assert_eq!(about.system_id, 7);
        assert!(about.gps_time);
        assert_eq!(
            about.firmware.as_deref(),
            Some("ArduCopter V4.5.1 (abcdef12)")
        );
        let records = crate::tlog::read(&tlog);
        // 2389 weeks and 100 000 s after the GPS epoch, less the leap seconds.
        let gps_at_2s = (GPS_EPOCH_S + 2389 * 604_800 - LEAP_S + 100_000) * 1_000_000 + 2_000_000;
        let first = records.first().unwrap().0;
        assert!(crate::tlog::sane(first));
        // The first GPS record, 2 s after boot, is at its own GPS time.
        assert!(records.iter().any(|(at, _)| *at == gps_at_2s));
        let mut session = crate::session::Session::new(crate::session::Config::default());
        let mut armed = Vec::new();
        let mut vibration = false;
        for (at, frame) in &records {
            let mut effects = session.ingest("log", "log", frame, *at);
            effects.extend(session.tick(*at));
            for effect in effects {
                match effect {
                    crate::session::Effect::Vehicle(seen) => {
                        assert_eq!(seen.system_id, 7);
                        if armed.last() != Some(&(seen.armed, seen.mode.clone())) {
                            armed.push((seen.armed, seen.mode.clone()));
                        }
                    }
                    crate::session::Effect::Telemetry(t) => {
                        vibration |= t.items.iter().any(|i| i.key.starts_with("vibration"));
                    }
                    _ => {}
                }
            }
        }
        assert!(armed.contains(&(true, Some("GUIDED".into()))), "{armed:?}");
        assert!(armed.contains(&(true, Some("RTL".into()))), "{armed:?}");
        assert_eq!(armed.last(), Some(&(false, Some("RTL".into()))));
        assert!(vibration, "VIBE becomes vibration");
    }

    #[test]
    fn a_log_without_gps_counts_from_boot() {
        let mut w = Writer::default();
        w.format(184, "EV", "QB", "TimeUS,Id");
        w.record("EV", &[Value::Int(5_000_000), Value::Int(10)]);
        let (tlog, about) = to_tlog(&w.bytes).unwrap();
        assert!(!about.gps_time);
        assert!(!crate::tlog::sane(crate::tlog::read(&tlog)[0].0));
        assert!(to_tlog(&[0u8; 100]).is_err());
    }

    #[test]
    fn the_producer_gives_parameters_home_motors_batteries_and_gps_time() {
        let (tlog, about) = to_tlog(&flight()).unwrap();
        let mut session = crate::session::Session::new(crate::session::Config::default());
        let mut parameters = BTreeMap::new();
        let mut keys = std::collections::BTreeSet::new();
        let mut offsets = Vec::new();
        for (at, frame) in crate::tlog::read(&tlog) {
            if crate::framing::header(frame).is_some_and(|h| h.message_id == 2) {
                let payload = crate::framing::payload(frame).unwrap();
                let mut unix = [0u8; 8];
                unix.copy_from_slice(&payload[..8]);
                offsets.push(i128::from(u64::from_le_bytes(unix)) - i128::from(at));
            }
            let mut effects = session.ingest("log", "log", frame, at);
            effects.extend(session.tick(at));
            for effect in effects {
                match effect {
                    crate::session::Effect::ParamValue { param, .. } => {
                        parameters.insert(param.name, param.value);
                    }
                    crate::session::Effect::Telemetry(t) => {
                        keys.extend(t.items.into_iter().map(|i| i.key));
                    }
                    _ => {}
                }
            }
        }
        assert_eq!(parameters.get("RTL_ALT"), Some(&2500.0));
        assert_eq!(parameters.get("SYSID_THISMAV"), Some(&7.0));
        for key in [
            "home.latitude_deg",
            "servo.1",
            "servo.4",
            "battery_2.temperature_degc",
        ] {
            assert!(keys.contains(key), "{key} in {keys:?}");
        }
        assert!(!keys.contains("servo.5"), "an unused output stays out");
        assert!(
            !offsets.is_empty() && offsets.iter().all(|o| *o == 0),
            "SYSTEM_TIME is the stamp"
        );
        let commands: Vec<(u16, u16)> = about.mission.iter().map(|m| (m.seq, m.command)).collect();
        assert_eq!(commands, [(0, 16), (1, 22), (2, 16)]);
        assert_eq!(about.mission[2].x, 515_000_300);
        assert!((about.mission[1].z - 20.0).abs() < 1e-6);
    }

    #[test]
    fn every_message_is_listed_with_its_instances_units_and_rate() {
        let messages = messages(&flight());
        let names: Vec<&str> = messages.iter().map(|m| m.name.as_str()).collect();
        assert!(
            names.contains(&"RCOU") && names.contains(&"CMD"),
            "{names:?}"
        );
        assert!(
            !names.contains(&"FMTU") && !names.contains(&"UNIT"),
            "{names:?}"
        );
        let vibe = messages.iter().find(|m| m.name == "VIBE").unwrap();
        assert_eq!(vibe.instances, [0, 1]);
        assert!(
            (vibe.rate_hz - 4.0).abs() < 0.1,
            "two instances at 2 Hz: {}",
            vibe.rate_hz
        );
        assert!(
            vibe.columns
                .iter()
                .all(|c| c.name != "IMU" && c.name != "TimeUS")
        );
        let bat = messages.iter().find(|m| m.name == "BAT").unwrap();
        let volt = bat.columns.iter().find(|c| c.name == "Volt").unwrap();
        assert_eq!(volt.unit.as_deref(), Some("V"));
        let gps = messages.iter().find(|m| m.name == "GPS").unwrap();
        assert_eq!(gps.instances, [0], "GPS's I is its instance");
    }

    #[test]
    fn a_series_is_as_logged_at_its_own_rate_on_the_replays_clock() {
        let bytes = flight();
        let keys: Vec<String> = [
            "df.VIBE[1].VibeZ",
            "df.VIBE.VibeZ",
            "df.RCOU.C1",
            "df.GPS.Lat",
            "df.NOPE.X",
            "vibration.z",
        ]
        .map(String::from)
        .to_vec();
        let series = series(&bytes, &keys);
        let second = &series["df.VIBE[1].VibeZ"];
        assert!(second.iter().all(|(_, v)| (*v - 45.0).abs() < 1e-6));
        assert!(
            series["df.VIBE.VibeZ"]
                .iter()
                .all(|(_, v)| (*v - 30.0).abs() < 1e-6)
        );
        // RCOU every tenth of a second, from 2 s to 60 s: no thinning.
        assert_eq!(series["df.RCOU.C1"].len(), 581);
        assert!((series["df.GPS.Lat"][0].1 - 51.500_002).abs() < 1e-9);
        assert!(!series.contains_key("df.NOPE.X") && !series.contains_key("vibration.z"));
        // The same clock as the producer: the first GPS record at its own GPS time.
        let (tlog, _) = to_tlog(&bytes).unwrap();
        let gps_at_2s = (GPS_EPOCH_S + 2389 * 604_800 - LEAP_S + 100_000) * 1_000_000 + 2_000_000;
        assert_eq!(series["df.GPS.Lat"][0].0, gps_at_2s);
        assert!(
            crate::tlog::read(&tlog)
                .iter()
                .any(|(at, _)| *at == gps_at_2s)
        );
        assert_eq!(parse_key("df.XKF4[1].SV"), Some(("XKF4", Some(1), "SV")));
        assert_eq!(parse_key("df.XKF4[x].SV"), None);
    }
}

/// A real SITL log, not kept in the repository: `UXDUCK_BIN=<path> cargo
/// test -p uxduck-mavlink --lib dataflash -- --ignored --nocapture`.
#[cfg(test)]
#[test]
#[ignore = "needs a log on this computer"]
fn a_real_sitl_log_plays() {
    use std::io::Read;
    let Ok(path) = std::env::var("UXDUCK_BIN") else {
        return;
    };
    let mut bytes = Vec::new();
    let _ = std::fs::File::open(path).map(|f| f.take(64 << 20).read_to_end(&mut bytes));
    let started = std::time::Instant::now();
    let (tlog, about) = to_tlog(&bytes).unwrap_or_default();
    let records = crate::tlog::read(&tlog);
    println!(
        "{} MB read, {} records out in {:?}, {about:?}, first {:?}",
        bytes.len() >> 20,
        records.len(),
        started.elapsed(),
        records.first().map(|r| r.0)
    );
    assert!(!records.is_empty());
}

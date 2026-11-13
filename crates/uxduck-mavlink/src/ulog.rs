//! PX4's ULog logs (`.ulg`), read with a reader of UXDuck's own as
//! [`crate::dataflash`] reads ArduPilot's: `F` messages describe every
//! topic, so the file says how to read itself, and the browser's engine
//! reads them unchanged.
//!
//! [`to_tlog`] is the producer for the map and instruments: it turns
//! `vehicle_status`, `vehicle_global_position`, `vehicle_local_position`,
//! `vehicle_gps_position`, `vehicle_attitude`, `battery_status`,
//! `home_position`, `vehicle_land_detected`, `vtol_vehicle_status`,
//! `airspeed_validated`, `actuator_outputs`, `wind`, `event`, the
//! parameters and the logged messages into the MAVLink messages a live
//! PX4 sends, stamped on the log's clock, so a replay of a `.ulg` is a
//! replay of a tlog. The clock is UTC from the GPS where the log has a
//! fix, else time since boot. A ULog holds no mission.
//!
//! [`messages`] and [`series`] read every topic as logged, at its own
//! rate, for the Chart and the agent, with a DataFlash field's keys:
//! `df.<topic>.<field>`, or `df.<topic>[<instance>].<field>` for a topic
//! of several instances.

use std::collections::{BTreeMap, BTreeSet};

use mavio::dialects::ardupilotmega::enums::{
    GpsFixType, MavAutopilot, MavLandedState, MavModeFlag, MavParamType, MavProtocolCapability,
    MavState, MavType, MavVtolState,
};
use mavio::dialects::ardupilotmega::messages::{
    Attitude, AutopilotVersion, BatteryStatus, Event, ExtendedSysState, GlobalPositionInt,
    GpsRawInt, Heartbeat, HomePosition, ParamValue, ServoOutputRaw, SysStatus, SystemTime, VfrHud,
    WindCov,
};

use crate::dataflash::{About, Column, Message, Value, int, parse_key};
use crate::sim::{frame, status_text_frame};

const MAGIC: [u8; 7] = *b"ULog\x01\x12\x35";
/// A sync message's bytes, where reading starts again after a corrupt
/// stretch.
const SYNC: [u8; 8] = [0x2F, 0x73, 0x13, 0x20, 0x25, 0x0C, 0xBB, 0x12];
/// The message types a ULog has.
const KINDS: &[u8] = b"BFIMPQARDLCSO";
/// `vehicle_status`'s `arming_state` when armed.
const ARMED: u8 = 2;
/// `vehicle_status`'s `vehicle_type` for a fixed wing.
const FIXED_WING: u8 = 2;
/// A `STATUSTEXT` part, and the most parts the session joins.
const TEXT_PART: usize = 50;
const TEXT_PARTS: usize = 8;

/// Whether bytes look like a ULog: its header's magic.
#[must_use]
pub fn is_ulog(bytes: &[u8]) -> bool {
    bytes.starts_with(&MAGIC)
}

/// A field's type as a format names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    F32,
    F64,
    Bool,
    Char,
}

impl Kind {
    fn of(name: &str) -> Option<Self> {
        Some(match name {
            "int8_t" => Self::I8,
            "uint8_t" => Self::U8,
            "int16_t" => Self::I16,
            "uint16_t" => Self::U16,
            "int32_t" => Self::I32,
            "uint32_t" => Self::U32,
            "int64_t" => Self::I64,
            "uint64_t" => Self::U64,
            "float" => Self::F32,
            "double" => Self::F64,
            "bool" => Self::Bool,
            "char" => Self::Char,
            _ => return None,
        })
    }

    const fn size(self) -> usize {
        match self {
            Self::I8 | Self::U8 | Self::Bool | Self::Char => 1,
            Self::I16 | Self::U16 => 2,
            Self::I32 | Self::U32 | Self::F32 => 4,
            Self::I64 | Self::U64 | Self::F64 => 8,
        }
    }

    /// A value of this type at the start of `raw`: `len` characters of
    /// text for `char`.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        reason = "each type's own width"
    )]
    fn read(self, raw: &[u8], len: usize) -> Option<Value> {
        if self == Self::Char {
            let raw = raw.get(..len)?;
            let end = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
            return Some(Value::Text(
                String::from_utf8_lossy(&raw[..end]).into_owned(),
            ));
        }
        let raw = raw.get(..self.size())?;
        let mut le = [0u8; 8];
        le[..raw.len()].copy_from_slice(raw);
        let bits = u64::from_le_bytes(le);
        Some(match self {
            Self::I8 => Value::Int(i64::from(bits as u8 as i8)),
            Self::U8 | Self::Char => Value::Int(i64::from(bits as u8)),
            Self::Bool => Value::Int(i64::from(bits != 0)),
            Self::I16 => Value::Int(i64::from(bits as u16 as i16)),
            Self::U16 => Value::Int(i64::from(bits as u16)),
            Self::I32 => Value::Int(i64::from(bits as u32 as i32)),
            Self::U32 => Value::Int(i64::from(bits as u32)),
            Self::I64 | Self::U64 => Value::Int(bits as i64),
            Self::F32 => Value::Float(f64::from(f32::from_bits(bits as u32))),
            Self::F64 => Value::Float(f64::from_bits(bits)),
        })
    }
}

/// A type with its count: `float[4]` is four floats; whether it is an
/// array.
fn kind_and_count(kind: &str) -> Option<(&str, usize, bool)> {
    match kind.split_once('[') {
        Some((kind, count)) => Some((kind, count.strip_suffix(']')?.parse().ok()?, true)),
        None => Some((kind, 1, false)),
    }
}

/// One field of a topic as logged, arrays and nested topics flattened
/// (`q[0]`, `esc[2].esc_rpm`); a `char` array is one field of text.
#[derive(Clone, Debug)]
pub struct Field {
    pub name: String,
    kind: Kind,
    /// Characters, for text.
    len: usize,
    /// Where it starts in a message.
    at: usize,
}

/// A topic's fields as its `F` message gives them.
#[derive(Clone, Debug, Default)]
pub struct Format {
    pub fields: Vec<Field>,
}

impl Format {
    /// A field's value in a message; none past its end, where PX4 leaves
    /// out the padding.
    #[must_use]
    pub fn value(&self, payload: &[u8], name: &str) -> Option<Value> {
        let field = self.fields.iter().find(|f| f.name == name)?;
        field.kind.read(payload.get(field.at..)?, field.len)
    }

    fn num(&self, payload: &[u8], name: &str) -> Option<f64> {
        self.value(payload, name)?.as_f64()
    }

    /// A field's number, when it is a finite one.
    fn finite(&self, payload: &[u8], name: &str) -> Option<f64> {
        self.num(payload, name).filter(|v| v.is_finite())
    }
}

/// A format's fields from its definition (`uint64_t timestamp;float[4] q;`)
/// and the formats it nests, from `at`; its size, or none when it names a
/// type the log does not define.
fn flatten(
    raw: &BTreeMap<String, String>,
    name: &str,
    prefix: &str,
    at: usize,
    depth: usize,
    out: &mut Vec<Field>,
) -> Option<usize> {
    if depth > 8 {
        return None;
    }
    let mut end = at;
    for field in raw.get(name)?.split(';').filter(|f| !f.trim().is_empty()) {
        let (kind, field_name) = field.trim().split_once(' ')?;
        let (kind, count, array) = kind_and_count(kind)?;
        // `_padding0` takes room but is no field.
        let padding = field_name.starts_with("_padding");
        let name = |i: usize| {
            if array {
                format!("{prefix}{field_name}[{i}]")
            } else {
                format!("{prefix}{field_name}")
            }
        };
        match Kind::of(kind) {
            Some(Kind::Char) => {
                if !padding {
                    out.push(Field {
                        name: format!("{prefix}{field_name}"),
                        kind: Kind::Char,
                        len: count,
                        at: end,
                    });
                }
                end += count;
            }
            Some(kind) => {
                if !padding {
                    out.extend((0..count).map(|i| Field {
                        name: name(i),
                        kind,
                        len: 1,
                        at: end + i * kind.size(),
                    }));
                }
                end += count * kind.size();
            }
            None => {
                for i in 0..count {
                    end += flatten(raw, kind, &format!("{}.", name(i)), end, depth + 1, out)?;
                }
            }
        }
    }
    Some(end - at)
}

/// Each message after the header: its type and its body. A corrupt
/// stretch is skipped to the next sync message, and data appended after
/// a crash is read from where the flags say it begins.
struct Messages<'a> {
    bytes: &'a [u8],
    at: usize,
    appended: Vec<usize>,
    /// Bytes at the end that are not a whole message: a log cut short.
    cut: usize,
}

impl<'a> Messages<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        // The flags come first: data is appended at their offsets when the
        // first incompatible flag's first bit is set.
        let mut appended: Vec<usize> = Vec::new();
        if bytes.get(18) == Some(&b'B') && bytes.get(27).is_some_and(|f| f & 1 == 1) {
            appended = (0..3)
                .filter_map(|i| {
                    let raw: [u8; 8] = bytes.get(35 + i * 8..43 + i * 8)?.try_into().ok()?;
                    usize::try_from(u64::from_le_bytes(raw))
                        .ok()
                        .filter(|at| *at > 0)
                })
                .collect();
            appended.sort_unstable();
        }
        Self {
            bytes,
            at: MAGIC.len() + 9,
            appended,
            cut: 0,
        }
    }
}

impl<'a> Iterator for Messages<'a> {
    type Item = (u8, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            while self.appended.first().is_some_and(|next| self.at >= *next) {
                self.appended.remove(0);
            }
            let header = self.bytes.get(self.at..self.at + 3)?;
            let size = usize::from(u16::from_le_bytes([header[0], header[1]]));
            let kind = header[2];
            let end = self.at + 3 + size;
            if let Some(&next) = self.appended.first()
                && end > next
            {
                self.at = next;
                continue;
            }
            if !KINDS.contains(&kind) {
                let from = self.at + 1;
                let sync = self.bytes[from..]
                    .windows(SYNC.len())
                    .position(|w| w == SYNC);
                self.at = sync.map_or(self.bytes.len(), |i| from + i + SYNC.len());
                continue;
            }
            let Some(body) = self.bytes.get(self.at + 3..end) else {
                self.cut = self.bytes.len() - self.at;
                self.at = self.bytes.len();
                return None;
            };
            self.at = end;
            return Some((kind, body));
        }
    }
}

/// An `I`, `P` or `Q` message's name and value: `<key length><type and
/// name><value>`.
fn key_value(body: &[u8]) -> Option<(String, Value)> {
    let len = usize::from(*body.first()?);
    let key = std::str::from_utf8(body.get(1..1 + len)?).ok()?;
    let (kind, name) = key.split_once(' ')?;
    let (kind, count, _) = kind_and_count(kind)?;
    let value = Kind::of(kind)?.read(body.get(1 + len..)?, count)?;
    Some((name.to_owned(), value))
}

/// A log's definitions: its topics' formats, what each message id is,
/// what it says of itself and its parameters.
#[derive(Debug, Default)]
pub struct Log {
    /// When logging began, in microseconds since boot.
    pub start_us: u64,
    pub formats: BTreeMap<String, Format>,
    /// Each subscription's topic and instance, by its message id.
    pub topics: BTreeMap<u16, (String, u8)>,
    /// `I` messages by name: `sys_name`, `ver_sw_release`, `ver_hw`...
    pub info: BTreeMap<String, Value>,
    /// Each parameter as first logged, in order.
    pub parameters: Vec<(String, Value)>,
    /// Each parameter's default as its airframe sets it, where the log
    /// gives defaults (PX4 1.11 onwards).
    pub defaults: BTreeMap<String, f32>,
    /// Bytes at the end that are not a whole message: a log cut short.
    pub cut: usize,
}

/// Reads a log's definitions: every format, subscription, information,
/// parameter and default.
///
/// PX4 logs a default only where it differs from the parameter's value:
/// its airframe's (`Q` bit 1) and its own (bit 0). A parameter with
/// neither, or only PX4's own, is at its airframe's default.
#[must_use]
#[allow(clippy::cast_possible_truncation, reason = "a parameter's value")]
pub fn read(bytes: &[u8]) -> Log {
    let mut log = Log {
        start_us: bytes
            .get(8..16)
            .and_then(|b| b.try_into().ok())
            .map_or(0, u64::from_le_bytes),
        ..Log::default()
    };
    let mut raw: BTreeMap<String, String> = BTreeMap::new();
    let mut named: BTreeSet<String> = BTreeSet::new();
    let mut airframe: BTreeMap<String, f32> = BTreeMap::new();
    let mut has_defaults = false;
    let mut messages = Messages::new(bytes);
    for (kind, body) in messages.by_ref() {
        match kind {
            b'F' => {
                if let Some((name, fields)) = std::str::from_utf8(body)
                    .ok()
                    .and_then(|t| t.split_once(':'))
                {
                    raw.insert(name.to_owned(), fields.to_owned());
                }
            }
            b'I' => {
                if let Some((name, value)) = key_value(body) {
                    log.info.insert(name, value);
                }
            }
            b'P' => {
                if let Some((name, value)) = key_value(body)
                    && named.insert(name.clone())
                {
                    log.parameters.push((name, value));
                }
            }
            b'Q' => {
                has_defaults = true;
                if let Some((&types, rest)) = body.split_first()
                    && types & 2 != 0
                    && let Some((name, value)) = key_value(rest)
                    && let Some(value) = value.as_f64()
                {
                    airframe.insert(name, value as f32);
                }
            }
            b'A' => {
                if let (Some(&multi), Some(id), Ok(name)) = (
                    body.first(),
                    body.get(1..3),
                    std::str::from_utf8(body.get(3..).unwrap_or_default()),
                ) {
                    log.topics
                        .insert(u16::from_le_bytes([id[0], id[1]]), (name.to_owned(), multi));
                }
            }
            _ => {}
        }
    }
    log.cut = messages.cut;
    if has_defaults {
        log.defaults = log
            .parameters
            .iter()
            .filter_map(|(name, value)| {
                let default = airframe
                    .get(name)
                    .copied()
                    .or_else(|| value.as_f64().map(|v| v as f32))?;
                Some((name.clone(), default))
            })
            .collect();
    }
    for name in raw.keys() {
        let mut fields = Vec::new();
        if flatten(&raw, name, "", 0, 0, &mut fields).is_some() {
            log.formats.insert(name.clone(), Format { fields });
        }
    }
    log
}

/// A `D` message's topic, instance, format and payload.
fn data<'a>(log: &'a Log, kind: u8, body: &'a [u8]) -> Option<(&'a str, u8, &'a Format, &'a [u8])> {
    if kind != b'D' {
        return None;
    }
    let id = u16::from_le_bytes([*body.first()?, *body.get(1)?]);
    let (name, multi) = log.topics.get(&id)?;
    Some((name, *multi, log.formats.get(name)?, &body[2..]))
}

/// A message's time since boot, in microseconds.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a log's microseconds"
)]
fn boot_us(format: &Format, payload: &[u8]) -> Option<u64> {
    format
        .num(payload, "timestamp")
        .filter(|t| *t >= 0.0)
        .map(|t| t as u64)
}

/// The GPS topic: the one PX4 flies on, else its receiver's own.
fn gps_topic(log: &Log) -> &'static str {
    if log
        .topics
        .values()
        .any(|(n, _)| n == "vehicle_gps_position")
    {
        "vehicle_gps_position"
    } else {
        "sensor_gps"
    }
}

/// The log's clock against Unix time in microseconds: its GPS's first 3D
/// fix's UTC time against that message's time since boot, else the boot
/// time it logged.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a log's microseconds"
)]
fn utc_offset(log: &Log, bytes: &[u8]) -> Option<i128> {
    let gps = gps_topic(log);
    Messages::new(bytes)
        .find_map(|(kind, body)| {
            let (topic, multi, format, payload) = data(log, kind, body)?;
            if topic != gps || multi != 0 || format.num(payload, "fix_type")? < 3.0 {
                return None;
            }
            let utc = format.num(payload, "time_utc_usec")? as u64;
            crate::tlog::sane(utc)
                .then(|| i128::from(utc) - i128::from(boot_us(format, payload).unwrap_or(0)))
        })
        .or_else(|| {
            let utc = log.info.get("boot_time_utc_us")?.as_f64()? as u64;
            crate::tlog::sane(utc.saturating_add(log.start_us)).then_some(i128::from(utc))
        })
}

/// A time since boot on the log's clock; nothing before logging began, so
/// that the heartbeat naming the vehicle comes first.
fn stamp(boot: u64, start: u64, offset: i128) -> u64 {
    u64::try_from(i128::from(boot.max(start)) + offset).unwrap_or(0)
}

fn text<'a>(log: &'a Log, key: &str) -> Option<&'a str> {
    log.info
        .get(key)
        .and_then(Value::as_text)
        .filter(|t| !t.is_empty())
}

/// Its firmware's line, such as "PX4 v1.15.0 (abcdef12)", from
/// `ver_sw_release` and `ver_sw`.
fn firmware(log: &Log) -> Option<String> {
    let name = text(log, "sys_name").unwrap_or("PX4");
    let release = log
        .info
        .get("ver_sw_release")
        .and_then(Value::as_f64)
        .map_or(0, int::<u32>);
    let [major, minor, patch, kind] = release.to_be_bytes();
    let version = (release >> 8 != 0).then(|| {
        let kind = match kind {
            255 => "",
            192.. => "-rc",
            128.. => "-beta",
            64.. => "-alpha",
            _ => "-dev",
        };
        format!("v{major}.{minor}.{patch}{kind}")
    });
    let hash = text(log, "ver_sw").map(|h| h.get(..8).unwrap_or(h));
    match (version, hash) {
        (Some(version), Some(hash)) => Some(format!("{name} {version} ({hash})")),
        (Some(version), None) => Some(format!("{name} {version}")),
        (None, Some(hash)) => Some(format!("{name} ({hash})")),
        (None, None) => None,
    }
}

/// PX4's `custom_mode` for a `vehicle_status` `nav_state`: its main mode
/// in the third byte, its sub mode in the fourth, as its MAVLink sends.
fn custom_mode(nav_state: u8) -> u32 {
    let (main, sub): (u32, u32) = match nav_state {
        0 | 13 => (1, 0),
        1 | 8 => (2, 0),
        2 => (3, 0),
        3 => (4, 4),
        4 => (4, 3),
        5 => (4, 5),
        6 => (3, 2),
        10 => (5, 0),
        12 | 18 => (4, 6),
        14 => (6, 0),
        15 => (7, 0),
        17 => (4, 2),
        19 => (4, 8),
        20 => (4, 9),
        21 => (3, 1),
        22 => (4, 10),
        23..=30 => (4, 11 + u32::from(nav_state - 23)),
        _ => (0, 0),
    };
    main << 16 | sub << 24
}

/// What the heartbeat and `EXTENDED_SYS_STATE` say, as last logged.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Status {
    armed: bool,
    nav_state: u8,
    mav_type: MavType,
    vtol: MavVtolState,
    landed: MavLandedState,
}

impl Status {
    /// From a `vehicle_status` message; its VTOL state too when the log
    /// has no `vtol_vehicle_status`. Whether anything changed.
    fn read(&mut self, format: &Format, payload: &[u8], vtol: bool) -> bool {
        let before = *self;
        let byte = |name: &str| format.num(payload, name).map(int::<u8>);
        if let Some(state) = byte("arming_state") {
            self.armed = state == ARMED;
        }
        if let Some(state) = byte("nav_state") {
            self.nav_state = state;
        }
        if let Some(kind) = byte("system_type").and_then(|t| MavType::try_from(t).ok())
            && kind != MavType::Generic
        {
            self.mav_type = kind;
        }
        if vtol && byte("is_vtol").is_some_and(|v| v > 0) {
            let fixed_wing = byte("vehicle_type").map_or_else(
                || byte("is_rotary_wing") == Some(0),
                |kind| kind == FIXED_WING,
            );
            self.vtol = if byte("in_transition_mode").is_some_and(|t| t > 0) {
                if byte("in_transition_to_fw").is_some_and(|t| t > 0) {
                    MavVtolState::TransitionToFw
                } else {
                    MavVtolState::TransitionToMc
                }
            } else if fixed_wing {
                MavVtolState::Fw
            } else {
                MavVtolState::Mc
            };
        }
        *self != before
    }

    fn heartbeat(self) -> Heartbeat {
        let mut base_mode = MavModeFlag::CUSTOM_MODE_ENABLED;
        if self.armed {
            base_mode |= MavModeFlag::SAFETY_ARMED;
        }
        Heartbeat {
            type_: self.mav_type,
            autopilot: MavAutopilot::Px4,
            base_mode,
            custom_mode: custom_mode(self.nav_state),
            system_status: if self.armed {
                MavState::Active
            } else {
                MavState::Standby
            },
            mavlink_version: 3,
        }
    }

    const fn extended(self) -> ExtendedSysState {
        ExtendedSysState {
            vtol_state: self.vtol,
            landed_state: self.landed,
        }
    }
}

/// A parameter as PX4 sends it: an integer's bits in the float.
#[allow(clippy::cast_possible_truncation, reason = "a parameter's float")]
fn param_value(name: &str, value: &Value, count: u16, index: u16) -> ParamValue {
    let mut param_id = [0u8; 16];
    let len = name.len().min(16);
    param_id[..len].copy_from_slice(&name.as_bytes()[..len]);
    let (param_value, param_type) = match value {
        Value::Int(v) => (
            f32::from_bits(i32::try_from(*v).unwrap_or_default().cast_unsigned()),
            MavParamType::Int32,
        ),
        _ => (
            value.as_f64().unwrap_or_default() as f32,
            MavParamType::Real32,
        ),
    };
    ParamValue {
        param_id,
        param_value,
        param_type,
        param_count: count,
        param_index: index,
    }
}

/// A logged message as PX4 sends it to a ground station: without the
/// module that wrote it (`[commander] `), its colours and its trailing
/// tabs.
fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    let out = out.trim();
    match out.strip_prefix('[').and_then(|rest| rest.split_once("] ")) {
        Some((module, rest)) if !module.contains(' ') => rest.trim().to_owned(),
        _ => out.to_owned(),
    }
}

/// A text in `STATUSTEXT`'s 50-byte parts, at most eight, with an empty
/// one after a last part that fills its 50, so that the text ends.
fn parts(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    loop {
        let mut cut = rest.len().min(TEXT_PART);
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        out.push(&rest[..cut]);
        rest = &rest[cut..];
        if rest.is_empty() || out.len() == TEXT_PARTS {
            break;
        }
    }
    if out.len() > 1 && out.len() < TEXT_PARTS && out.last().is_some_and(|p| p.len() == TEXT_PART) {
        out.push("");
    }
    out
}

/// Radians as MAVLink's centidegrees from north, 0 to 35 999.
fn centidegrees(radians: f64) -> u16 {
    int::<u16>(radians.to_degrees().rem_euclid(360.0) * 100.0) % 36_000
}

/// The log as tlog records on its own clock, and what it said of its
/// vehicle.
///
/// # Errors
/// When it is not a ULog, or holds no topics UXDuck can read.
#[allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "one arm per topic"
)]
pub fn to_tlog(bytes: &[u8]) -> Result<(Vec<u8>, About), String> {
    if !is_ulog(bytes) {
        return Err("This is not a ULog.".into());
    }
    let log = read(bytes);
    if log.topics.is_empty() {
        return Err("This log holds no records UXDuck can read.".into());
    }
    let parameter = |name: &str| {
        log.parameters
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, v)| v.as_f64())
    };
    let offset = utc_offset(&log, bytes);
    let about = About {
        system_id: parameter("MAV_SYS_ID").map_or(1, int::<u8>).max(1),
        firmware: firmware(&log),
        gps_time: offset.is_some(),
        mission: Vec::new(),
        defaults: log.defaults.clone(),
    };
    let offset = offset.unwrap_or(0);
    let start = log.start_us;
    let system = about.system_id;
    let has = |topic: &str| log.topics.values().any(|(n, _)| n == topic);
    let gps = gps_topic(&log);
    let has_position = has("vehicle_global_position");
    let vtol_from_status = !has("vtol_vehicle_status");
    let mut out: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut sequence = 0u8;
    let mut next = || {
        sequence = sequence.wrapping_add(1);
        sequence
    };
    // Its state from the start, rather than disarmed in mode 0 until its
    // status is logged.
    let mut status = Status {
        armed: false,
        nav_state: 0,
        mav_type: parameter("MAV_TYPE")
            .and_then(|t| MavType::try_from(int::<u8>(t)).ok())
            .unwrap_or(MavType::Generic),
        vtol: MavVtolState::Undefined,
        landed: MavLandedState::Undefined,
    };
    if let Some((format, payload)) = Messages::new(bytes).find_map(|(kind, body)| {
        let (topic, multi, format, payload) = data(&log, kind, body)?;
        (topic == "vehicle_status" && multi == 0).then_some((format, payload))
    }) {
        status.read(format, payload, vtol_from_status);
    }
    let first = stamp(start, start, offset);
    out.push((first, frame(system, 1, next(), &status.heartbeat())));
    // Its git hash as ArduPilot sends one, eight hex digits.
    let mut flight_custom_version = [0u8; 8];
    if let Some(hash) = text(&log, "ver_sw").and_then(|h| h.get(..8)) {
        flight_custom_version.copy_from_slice(hash.as_bytes());
    }
    let version = AutopilotVersion {
        capabilities: MavProtocolCapability::PARAM_ENCODE_BYTEWISE,
        flight_sw_version: log
            .info
            .get("ver_sw_release")
            .and_then(Value::as_f64)
            .map_or(0, int::<u32>),
        flight_custom_version,
        ..AutopilotVersion::default()
    };
    out.push((first, frame(system, 1, next(), &version)));
    let count = u16::try_from(log.parameters.len()).unwrap_or(u16::MAX);
    let mut indices: BTreeMap<&str, u16> = BTreeMap::new();
    for (index, (name, value)) in log.parameters.iter().enumerate() {
        let index = u16::try_from(index).unwrap_or(u16::MAX);
        indices.insert(name, index);
        let param = param_value(name, value, count, index);
        out.push((first, frame(system, 1, next(), &param)));
    }
    let mut data_started = false;
    let mut last_boot = start;
    let mut heartbeat_at = first;
    let mut sent: BTreeMap<&str, u64> = BTreeMap::new();
    let mut battery_at: BTreeMap<u8, u64> = BTreeMap::new();
    let mut first_battery: Option<u8> = None;
    let mut home: Option<(i32, i32, i32)> = None;
    let mut home_alt: Option<f64> = None;
    let mut local: Option<(&Format, &[u8])> = None;
    let mut rates: Option<(&Format, &[u8])> = None;
    let mut airspeed: Option<f64> = None;
    let mut text_id = 0u16;
    for (kind, body) in Messages::new(bytes) {
        match kind {
            b'A' => data_started = true,
            // A parameter changed in flight.
            b'P' if data_started => {
                if let Some((name, value)) = key_value(body) {
                    let index = indices.get(name.as_str()).copied().unwrap_or(u16::MAX);
                    let param = param_value(&name, &value, count, index);
                    out.push((
                        stamp(last_boot, start, offset),
                        frame(system, 1, next(), &param),
                    ));
                }
                continue;
            }
            // A logged message; a tagged one has its tag first.
            b'L' | b'C' => {
                let skip = if kind == b'C' { 2 } else { 0 };
                let (Some(&level), Some(time)) = (body.first(), body.get(1 + skip..9 + skip))
                else {
                    continue;
                };
                let mut le = [0u8; 8];
                le.copy_from_slice(time);
                let severity = if level >= b'0' { level - b'0' } else { level };
                let text = plain(&String::from_utf8_lossy(&body[9 + skip..]));
                if severity > 6 || text.is_empty() {
                    continue;
                }
                let at = stamp(u64::from_le_bytes(le), start, offset);
                let parts = parts(&text);
                let id = if parts.len() > 1 {
                    text_id = text_id.wrapping_add(1).max(1);
                    text_id
                } else {
                    0
                };
                for (chunk, part) in parts.into_iter().enumerate() {
                    let chunk = u8::try_from(chunk).unwrap_or(u8::MAX);
                    out.push((
                        at,
                        status_text_frame(system, next(), severity, part, (id, chunk)),
                    ));
                }
                continue;
            }
            _ => {}
        }
        let Some((topic, multi, format, payload)) = data(&log, kind, body) else {
            continue;
        };
        let Some(boot) = boot_us(format, payload) else {
            continue;
        };
        data_started = true;
        last_boot = last_boot.max(boot);
        let at = stamp(boot, start, offset);
        let time_boot_ms = u32::try_from(boot / 1000).unwrap_or(u32::MAX);
        let n = |name: &str| format.finite(payload, name);
        let mut changed = false;
        match (topic, multi) {
            ("vehicle_status", 0) => changed = status.read(format, payload, vtol_from_status),
            ("vtol_vehicle_status", 0) => {
                if let Some(state) =
                    n("vehicle_vtol_state").and_then(|s| MavVtolState::try_from(int::<u8>(s)).ok())
                {
                    changed = state != status.vtol;
                    status.vtol = state;
                }
            }
            ("vehicle_land_detected", 0) => {
                if let Some(landed) = n("landed") {
                    let landed = if landed > 0.0 {
                        MavLandedState::OnGround
                    } else {
                        MavLandedState::InAir
                    };
                    changed = landed != status.landed;
                    status.landed = landed;
                }
            }
            ("vehicle_local_position", 0) => local = Some((format, payload)),
            ("vehicle_angular_velocity", 0) => rates = Some((format, payload)),
            ("airspeed_validated", 0) => airspeed = n("calibrated_airspeed_m_s"),
            ("home_position", 0) if n("valid_hpos") != Some(0.0) => {
                if let (Some(lat), Some(lon)) = (n("lat"), n("lon")) {
                    let alt = n("alt").unwrap_or(0.0);
                    home_alt = Some(alt);
                    let place = (int(lat * 1e7), int(lon * 1e7), int(alt * 1000.0));
                    if home != Some(place) {
                        home = Some(place);
                        let position = HomePosition {
                            latitude: place.0,
                            longitude: place.1,
                            altitude: place.2,
                            x: n("x").unwrap_or(0.0) as f32,
                            y: n("y").unwrap_or(0.0) as f32,
                            z: n("z").unwrap_or(0.0) as f32,
                            q: [1.0, 0.0, 0.0, 0.0],
                            time_usec: at,
                            ..HomePosition::default()
                        };
                        out.push((at, frame(system, 1, next(), &position)));
                    }
                }
            }
            _ => {}
        }
        // A heartbeat each second, and at once when arming, the mode or
        // the flight state changes.
        if changed || at.saturating_sub(heartbeat_at) >= 1_000_000 {
            heartbeat_at = at;
            out.push((at, frame(system, 1, next(), &status.heartbeat())));
            out.push((at, frame(system, 1, next(), &status.extended())));
            if about.gps_time {
                let time = SystemTime {
                    time_unix_usec: at,
                    time_boot_ms,
                };
                out.push((at, frame(system, 1, next(), &time)));
            }
        }
        let due = |sent: &BTreeMap<&str, u64>, what: &str, every: u64| {
            sent.get(what)
                .is_none_or(|t| at.saturating_sub(*t) >= every)
        };
        let velocity = |name: &str| local.and_then(|(f, p)| f.finite(p, name));
        match (topic, multi) {
            ("vehicle_global_position", 0) if due(&sent, "position", 100_000) => {
                let (Some(lat), Some(lon)) = (n("lat"), n("lon")) else {
                    continue;
                };
                if n("lat_lon_valid") == Some(0.0) {
                    continue;
                }
                let alt = n("alt").unwrap_or(0.0);
                let (vx, vy, vz) = (
                    velocity("vx").unwrap_or(0.0),
                    velocity("vy").unwrap_or(0.0),
                    velocity("vz").unwrap_or(0.0),
                );
                let heading = velocity("heading");
                let above_home = home_alt
                    .map(|home| alt - home)
                    .or_else(|| velocity("z").map(|z| -z))
                    .unwrap_or(0.0);
                let position = GlobalPositionInt {
                    time_boot_ms,
                    lat: int(lat * 1e7),
                    lon: int(lon * 1e7),
                    alt: int(alt * 1000.0),
                    relative_alt: int(above_home * 1000.0),
                    vx: int(vx * 100.0),
                    vy: int(vy * 100.0),
                    vz: int(vz * 100.0),
                    hdg: heading.map_or(u16::MAX, centidegrees),
                };
                out.push((at, frame(system, 1, next(), &position)));
                let hud = VfrHud {
                    airspeed: airspeed.unwrap_or(0.0) as f32,
                    groundspeed: vx.hypot(vy) as f32,
                    heading: heading.map_or(0, |h| int(h.to_degrees().rem_euclid(360.0))),
                    alt: alt as f32,
                    climb: -vz as f32,
                    ..VfrHud::default()
                };
                out.push((at, frame(system, 1, next(), &hud)));
                sent.insert("position", at);
            }
            (t, 0) if t == gps && due(&sent, "gps", 200_000) => {
                // Degrees since PX4 1.14; degrees times 1e7 and millimetres
                // before.
                let degrees = |new: &str, old: &str| n(new).or_else(|| n(old).map(|v| v * 1e-7));
                let (Some(lat), Some(lon)) = (
                    degrees("latitude_deg", "lat"),
                    degrees("longitude_deg", "lon"),
                ) else {
                    continue;
                };
                let alt = n("altitude_msl_m")
                    .or_else(|| n("alt").map(|mm| mm / 1000.0))
                    .unwrap_or(0.0);
                let raw = GpsRawInt {
                    time_usec: at,
                    fix_type: GpsFixType::try_from(n("fix_type").map_or(0, int::<u8>))
                        .unwrap_or(GpsFixType::NoFix),
                    lat: int(lat * 1e7),
                    lon: int(lon * 1e7),
                    alt: int(alt * 1000.0),
                    eph: n("hdop").map_or(u16::MAX, |d| int(d * 100.0)),
                    epv: n("vdop").map_or(u16::MAX, |d| int(d * 100.0)),
                    vel: n("vel_m_s").map_or(u16::MAX, |v| int(v * 100.0)),
                    cog: n("cog_rad").map_or(u16::MAX, centidegrees),
                    satellites_visible: n("satellites_used").map_or(u8::MAX, int),
                    alt_ellipsoid: n("altitude_ellipsoid_m").map_or(0, |m| int(m * 1000.0)),
                    h_acc: n("eph").map_or(0, |m| int(m * 1000.0)),
                    v_acc: n("epv").map_or(0, |m| int(m * 1000.0)),
                    vel_acc: n("s_variance_m_s").map_or(0, |v| int(v * 1000.0)),
                    ..GpsRawInt::default()
                };
                out.push((at, frame(system, 1, next(), &raw)));
                if !has_position {
                    let position = GlobalPositionInt {
                        time_boot_ms,
                        lat: int(lat * 1e7),
                        lon: int(lon * 1e7),
                        alt: int(alt * 1000.0),
                        relative_alt: home_alt.map_or(0, |home| int((alt - home) * 1000.0)),
                        hdg: u16::MAX,
                        ..GlobalPositionInt::default()
                    };
                    out.push((at, frame(system, 1, next(), &position)));
                }
                sent.insert("gps", at);
            }
            ("vehicle_attitude", 0) if due(&sent, "attitude", 100_000) => {
                let q = |i: usize| n(&format!("q[{i}]"));
                let (Some(qw), Some(qx), Some(qy), Some(qz)) = (q(0), q(1), q(2), q(3)) else {
                    continue;
                };
                let rate = |i: usize| {
                    rates
                        .and_then(|(f, p)| f.finite(p, &format!("xyz[{i}]")))
                        .unwrap_or(0.0) as f32
                };
                let attitude = Attitude {
                    time_boot_ms,
                    roll: (2.0 * (qw * qx + qy * qz)).atan2(1.0 - 2.0 * (qx * qx + qy * qy)) as f32,
                    pitch: (2.0 * (qw * qy - qz * qx)).clamp(-1.0, 1.0).asin() as f32,
                    yaw: (2.0 * (qw * qz + qx * qy)).atan2(1.0 - 2.0 * (qy * qy + qz * qz)) as f32,
                    rollspeed: rate(0),
                    pitchspeed: rate(1),
                    yawspeed: rate(2),
                };
                out.push((at, frame(system, 1, next(), &attitude)));
                sent.insert("attitude", at);
            }
            ("battery_status", instance) => {
                if battery_at
                    .get(&instance)
                    .is_some_and(|t| at.saturating_sub(*t) < 1_000_000)
                    || n("connected") == Some(0.0)
                {
                    continue;
                }
                battery_at.insert(instance, at);
                let volts = n("voltage_v")
                    .or_else(|| n("voltage_filtered_v"))
                    .unwrap_or(0.0);
                let current = n("current_a")
                    .filter(|a| *a >= 0.0)
                    .map_or(-1, |a| int(a * 100.0));
                let remaining = n("remaining")
                    .filter(|r| *r >= 0.0)
                    .map_or(-1, |r| int((r * 100.0).min(100.0)));
                if *first_battery.get_or_insert(instance) == instance {
                    let status = SysStatus {
                        voltage_battery: int(volts * 1000.0),
                        current_battery: current,
                        battery_remaining: remaining,
                        ..SysStatus::default()
                    };
                    out.push((at, frame(system, 1, next(), &status)));
                }
                let mut voltages = [u16::MAX; 10];
                voltages[0] = int(volts * 1000.0);
                let battery = BatteryStatus {
                    id: instance,
                    temperature: n("temperature").map_or(i16::MAX, |c| int(c * 100.0)),
                    voltages,
                    current_battery: current,
                    current_consumed: n("discharged_mah").map_or(-1, int),
                    energy_consumed: -1,
                    battery_remaining: remaining,
                    time_remaining: n("time_remaining_s").map_or(0, int),
                    ..BatteryStatus::default()
                };
                out.push((at, frame(system, 1, next(), &battery)));
            }
            ("actuator_outputs", 0) if due(&sent, "servos", 100_000) => {
                let outputs = n("noutputs").map_or(16, int::<usize>);
                let pulse = |i: usize| {
                    if i < outputs {
                        n(&format!("output[{i}]")).map_or(0, int::<u16>)
                    } else {
                        0
                    }
                };
                let servos = ServoOutputRaw {
                    time_usec: u32::try_from(boot % (1 << 32)).unwrap_or(0),
                    port: 0,
                    servo1_raw: pulse(0),
                    servo2_raw: pulse(1),
                    servo3_raw: pulse(2),
                    servo4_raw: pulse(3),
                    servo5_raw: pulse(4),
                    servo6_raw: pulse(5),
                    servo7_raw: pulse(6),
                    servo8_raw: pulse(7),
                    servo9_raw: pulse(8),
                    servo10_raw: pulse(9),
                    servo11_raw: pulse(10),
                    servo12_raw: pulse(11),
                    servo13_raw: pulse(12),
                    servo14_raw: pulse(13),
                    servo15_raw: pulse(14),
                    servo16_raw: pulse(15),
                };
                out.push((at, frame(system, 1, next(), &servos)));
                sent.insert("servos", at);
            }
            // `wind_estimate` before PX4 1.13.
            ("wind" | "wind_estimate", 0) if due(&sent, "wind", 1_000_000) => {
                let (Some(north), Some(east)) = (n("windspeed_north"), n("windspeed_east")) else {
                    continue;
                };
                let wind = WindCov {
                    time_usec: at,
                    wind_x: north as f32,
                    wind_y: east as f32,
                    var_horiz: (n("variance_north").unwrap_or(0.0)
                        + n("variance_east").unwrap_or(0.0)) as f32,
                    ..WindCov::default()
                };
                out.push((at, frame(system, 1, next(), &wind)));
                sent.insert("wind", at);
            }
            ("event", _) => {
                let (Some(id), Some(sequence)) = (n("id"), n("event_sequence")) else {
                    continue;
                };
                let mut arguments = [0u8; 40];
                for (i, argument) in arguments.iter_mut().enumerate().take(25) {
                    *argument = n(&format!("arguments[{i}]")).map_or(0, int);
                }
                let event = Event {
                    id: int(id),
                    event_time_boot_ms: time_boot_ms,
                    sequence: int(sequence),
                    log_levels: n("log_levels").map_or(0, int),
                    arguments,
                    ..Event::default()
                };
                out.push((at, frame(system, 1, next(), &event)));
            }
            _ => {}
        }
    }
    // PX4 writes each topic as it reads it, a little out of order between
    // topics.
    out.sort_by_key(|(at, _)| *at);
    let mut tlog = Vec::new();
    for (at, frame_bytes) in out {
        let _ = crate::tlog::record(&mut tlog, at, &frame_bytes);
    }
    Ok((tlog, about))
}

/// A field's unit by PX4's naming (`voltage_v`, `vel_m_s`), where its name
/// says one.
fn unit(field: &str) -> Option<String> {
    let name = field.rsplit('.').next()?.split('[').next()?;
    [
        ("_m_s2", "m/s/s"),
        ("_m_s", "m/s"),
        ("_rad_s", "rad/s"),
        ("_rad", "rad"),
        ("_deg", "deg"),
        ("_mah", "mAh"),
        ("_wh", "Wh"),
        ("_pa", "Pa"),
        ("_hz", "Hz"),
        ("_us", "us"),
        ("_m", "m"),
        ("_v", "V"),
        ("_a", "A"),
        ("_s", "s"),
    ]
    .iter()
    .find(|(suffix, _)| name.ends_with(suffix))
    .map(|(_, unit)| (*unit).to_owned())
}

/// Every topic the log holds with numbers in it, as logged: what the
/// Chart offers and `replay/logFields` lists.
#[must_use]
#[allow(clippy::cast_precision_loss, reason = "record counts and times")]
pub fn messages(bytes: &[u8]) -> Vec<Message> {
    let log = read(bytes);
    let mut seen: BTreeMap<&str, (usize, u64, u64, BTreeSet<u8>)> = BTreeMap::new();
    for (kind, body) in Messages::new(bytes) {
        let Some((topic, multi, format, payload)) = data(&log, kind, body) else {
            continue;
        };
        let time = boot_us(format, payload).unwrap_or(0);
        let entry = seen
            .entry(topic)
            .or_insert((0, time, time, BTreeSet::new()));
        entry.0 += 1;
        entry.1 = entry.1.min(time);
        entry.2 = entry.2.max(time);
        entry.3.insert(multi);
    }
    seen.into_iter()
        .filter_map(|(topic, (records, first, last, instances))| {
            let columns: Vec<Column> = log
                .formats
                .get(topic)?
                .fields
                .iter()
                .filter(|f| f.kind != Kind::Char && f.name != "timestamp")
                .map(|f| Column {
                    name: f.name.clone(),
                    unit: unit(&f.name),
                })
                .collect();
            if columns.is_empty() {
                return None;
            }
            let span_s = (last - first) as f64 / 1e6;
            Some(Message {
                name: topic.to_owned(),
                columns,
                instances: if instances.len() > 1 {
                    instances.into_iter().map(i64::from).collect()
                } else {
                    Vec::new()
                },
                records,
                rate_hz: if span_s > 0.0 {
                    records.saturating_sub(1) as f64 / span_s
                } else {
                    0.0
                },
            })
        })
        .collect()
}

/// Each key's values as logged, on the log's clock (the same stamps
/// [`to_tlog`] gives), in the order written. A key the log does not hold
/// is left out, and a key with no instance reads the topic's first.
#[must_use]
pub fn series(bytes: &[u8], keys: &[String]) -> BTreeMap<String, Vec<(u64, f64)>> {
    let log = read(bytes);
    let offset = utc_offset(&log, bytes).unwrap_or(0);
    let mut wanted: BTreeMap<u16, Vec<(&String, &str)>> = BTreeMap::new();
    for key in keys {
        let Some((topic, instance, field)) = parse_key(key) else {
            continue;
        };
        let numeric = log.formats.get(topic).is_some_and(|format| {
            format
                .fields
                .iter()
                .any(|f| f.name == field && f.kind != Kind::Char)
        });
        let id = log
            .topics
            .iter()
            .filter(|(_, (name, multi))| {
                name == topic && instance.is_none_or(|i| i == i64::from(*multi))
            })
            .min_by_key(|(_, (_, multi))| *multi)
            .map(|(id, _)| *id);
        if let Some(id) = id
            && numeric
        {
            wanted.entry(id).or_default().push((key, field));
        }
    }
    let mut out: BTreeMap<String, Vec<(u64, f64)>> = BTreeMap::new();
    for (kind, body) in Messages::new(bytes) {
        if kind != b'D' || body.len() < 2 {
            continue;
        }
        let Some(fields) = wanted.get(&u16::from_le_bytes([body[0], body[1]])) else {
            continue;
        };
        let Some((_, _, format, payload)) = data(&log, kind, body) else {
            continue;
        };
        let Some(boot) = boot_us(format, payload) else {
            continue;
        };
        let at = stamp(boot, log.start_us, offset);
        for (key, field) in fields {
            if let Some(value) = format.finite(payload, field) {
                out.entry((*key).clone()).or_default().push((at, value));
            }
        }
    }
    out
}

/// ULog files made up for tests here and in the engine, as the fake
/// vehicle is for tlogs.
pub mod fixture {
    use super::{Field, Kind, MAGIC, SYNC, flatten};
    use std::collections::BTreeMap;

    /// A ULog writer for tests: its header, definitions, then data.
    #[derive(Debug)]
    pub struct Writer {
        pub bytes: Vec<u8>,
        formats: BTreeMap<String, String>,
        topics: BTreeMap<String, u16>,
    }

    impl Writer {
        /// A log that began `start_us` after boot.
        #[must_use]
        pub fn new(start_us: u64) -> Self {
            let mut bytes = MAGIC.to_vec();
            bytes.push(1);
            bytes.extend_from_slice(&start_us.to_le_bytes());
            let mut w = Self {
                bytes,
                formats: BTreeMap::new(),
                topics: BTreeMap::new(),
            };
            w.message(b'B', &[0u8; 40]);
            w
        }

        /// A message of `kind` with its body.
        pub fn message(&mut self, kind: u8, body: &[u8]) {
            let size = u16::try_from(body.len()).unwrap_or(u16::MAX);
            self.bytes.extend_from_slice(&size.to_le_bytes());
            self.bytes.push(kind);
            self.bytes.extend_from_slice(body);
        }

        /// A topic's format: `uint64_t timestamp;float[4] q;`.
        pub fn format(&mut self, name: &str, fields: &str) {
            self.message(b'F', format!("{name}:{fields}").as_bytes());
            self.formats.insert(name.to_owned(), fields.to_owned());
        }

        fn key_value(key: &str, value: &[u8]) -> Vec<u8> {
            let mut body = vec![u8::try_from(key.len()).unwrap_or(u8::MAX)];
            body.extend_from_slice(key.as_bytes());
            body.extend_from_slice(value);
            body
        }

        /// What the log says of itself, as text: `ver_sw`, `sys_name`.
        pub fn info_text(&mut self, name: &str, text: &str) {
            let body = Self::key_value(&format!("char[{}] {name}", text.len()), text.as_bytes());
            self.message(b'I', &body);
        }

        pub fn info_u32(&mut self, name: &str, value: u32) {
            let body = Self::key_value(&format!("uint32_t {name}"), &value.to_le_bytes());
            self.message(b'I', &body);
        }

        pub fn param_int(&mut self, name: &str, value: i32) {
            let body = Self::key_value(&format!("int32_t {name}"), &value.to_le_bytes());
            self.message(b'P', &body);
        }

        pub fn param_float(&mut self, name: &str, value: f32) {
            let body = Self::key_value(&format!("float {name}"), &value.to_le_bytes());
            self.message(b'P', &body);
        }

        /// A default: `types` 1 for PX4's, 2 for its airframe's.
        pub fn default_float(&mut self, types: u8, name: &str, value: f32) {
            let mut body = vec![types];
            body.extend(Self::key_value(
                &format!("float {name}"),
                &value.to_le_bytes(),
            ));
            self.message(b'Q', &body);
        }

        /// A subscription to a topic's instance, with the next message id.
        pub fn subscribe(&mut self, topic: &str, multi: u8) {
            let id = u16::try_from(self.topics.len()).unwrap_or(u16::MAX);
            let mut body = vec![multi];
            body.extend_from_slice(&id.to_le_bytes());
            body.extend_from_slice(topic.as_bytes());
            self.message(b'A', &body);
            self.topics.insert(format!("{topic}[{multi}]"), id);
        }

        /// A topic's message: the fields named, the rest zero, and its
        /// padding at the end left out as PX4 leaves it.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "test values"
        )]
        pub fn data(&mut self, topic: &str, multi: u8, values: &[(&str, f64)]) {
            let id = self.topics[&format!("{topic}[{multi}]")];
            let mut fields: Vec<Field> = Vec::new();
            let size = flatten(&self.formats, topic, "", 0, 0, &mut fields).unwrap_or(0);
            let mut payload = vec![0u8; size];
            for (name, value) in values {
                let Some(field) = fields.iter().find(|f| f.name == *name) else {
                    continue;
                };
                let bytes = match field.kind {
                    Kind::I8 | Kind::U8 | Kind::Bool | Kind::Char => vec![*value as i64 as u8],
                    Kind::I16 | Kind::U16 => (*value as i64 as u16).to_le_bytes().to_vec(),
                    Kind::I32 | Kind::U32 => (*value as i64 as u32).to_le_bytes().to_vec(),
                    Kind::I64 | Kind::U64 => (*value as u64).to_le_bytes().to_vec(),
                    Kind::F32 => (*value as f32).to_le_bytes().to_vec(),
                    Kind::F64 => value.to_le_bytes().to_vec(),
                };
                payload[field.at..field.at + bytes.len()].copy_from_slice(&bytes);
            }
            let end = fields
                .iter()
                .map(|f| f.at + f.kind.size() * f.len)
                .max()
                .unwrap_or(0);
            payload.truncate(end);
            let mut body = id.to_le_bytes().to_vec();
            body.extend_from_slice(&payload);
            self.message(b'D', &body);
        }

        /// A logged message: `level` as PX4 writes it, `'0'` to `'7'`.
        pub fn log(&mut self, level: u8, boot_us: u64, text: &str) {
            let mut body = vec![level];
            body.extend_from_slice(&boot_us.to_le_bytes());
            body.extend_from_slice(text.as_bytes());
            self.message(b'L', &body);
        }

        pub fn sync(&mut self) {
            self.message(b'S', &SYNC);
        }
    }

    /// A PX4 quadcopter's minute, logged from 1 s after boot: armed at
    /// 10 s in Position, then Return at 25 s, up to 20 m and back,
    /// disarmed at 50 s; GPS time, two batteries, its motors, home, wind,
    /// an event, its messages and a parameter changed in flight.
    #[must_use]
    #[allow(
        clippy::too_many_lines,
        clippy::cast_precision_loss,
        reason = "a made-up minute, message by message"
    )]
    pub fn flight() -> Vec<u8> {
        let mut w = Writer::new(1_000_000);
        w.info_text("sys_name", "PX4");
        w.info_text("ver_sw", "0123456789abcdef0123456789abcdef01234567");
        w.info_u32("ver_sw_release", 0x010F_00FF);
        w.info_text("ver_hw", "PX4_FMU_V6X");
        w.format(
            "vehicle_status",
            "uint64_t timestamp;uint8_t arming_state;uint8_t nav_state;uint8_t vehicle_type;\
             bool is_vtol;uint8_t system_type;uint8_t[3] _padding0;",
        );
        w.format(
            "vehicle_global_position",
            "uint64_t timestamp;double lat;double lon;float alt;bool lat_lon_valid;\
             uint8_t[3] _padding0;",
        );
        w.format(
            "vehicle_local_position",
            "uint64_t timestamp;float x;float y;float z;float vx;float vy;float vz;float heading;",
        );
        w.format(
            "vehicle_gps_position",
            "uint64_t timestamp;double latitude_deg;double longitude_deg;double altitude_msl_m;\
             uint64_t time_utc_usec;float eph;float epv;float hdop;float vdop;float vel_m_s;\
             float cog_rad;uint8_t fix_type;uint8_t satellites_used;uint8_t[6] _padding0;",
        );
        w.format("vehicle_attitude", "uint64_t timestamp;float[4] q;");
        w.format(
            "vehicle_angular_velocity",
            "uint64_t timestamp;float[3] xyz;",
        );
        w.format(
            "battery_status",
            "uint64_t timestamp;float voltage_v;float current_a;float discharged_mah;\
             float remaining;float temperature;bool connected;uint8_t[3] _padding0;",
        );
        w.format(
            "home_position",
            "uint64_t timestamp;double lat;double lon;float alt;bool valid_hpos;\
             uint8_t[3] _padding0;",
        );
        w.format(
            "vehicle_land_detected",
            "uint64_t timestamp;bool landed;uint8_t[7] _padding0;",
        );
        w.format(
            "actuator_outputs",
            "uint64_t timestamp;uint32_t noutputs;float[16] output;uint8_t[4] _padding0;",
        );
        w.format(
            "wind",
            "uint64_t timestamp;float windspeed_north;float windspeed_east;\
             float variance_north;float variance_east;",
        );
        w.format(
            "esc_report",
            "uint64_t timestamp;int32_t esc_rpm;float esc_voltage;",
        );
        w.format(
            "esc_status",
            "uint64_t timestamp;uint8_t esc_count;uint8_t[7] _padding0;esc_report[2] esc;",
        );
        w.format(
            "event",
            "uint64_t timestamp;uint32_t id;uint16_t event_sequence;uint8_t[25] arguments;\
             uint8_t log_levels;",
        );
        w.param_int("MAV_SYS_ID", 7);
        w.param_int("MAV_TYPE", 2);
        w.param_float("MPC_XY_VEL_MAX", 12.0);
        w.param_int("COM_RC_IN_MODE", 3);
        w.default_float(1, "MPC_XY_VEL_MAX", 12.0);
        w.default_float(2, "MPC_XY_VEL_MAX", 10.0);
        w.default_float(3, "COM_RC_IN_MODE", 0.0);
        w.default_float(1, "MAV_TYPE", 0.0);
        for (topic, multi) in [
            ("vehicle_status", 0),
            ("vehicle_global_position", 0),
            ("vehicle_local_position", 0),
            ("vehicle_gps_position", 0),
            ("vehicle_attitude", 0),
            ("vehicle_angular_velocity", 0),
            ("battery_status", 0),
            ("battery_status", 1),
            ("home_position", 0),
            ("vehicle_land_detected", 0),
            ("actuator_outputs", 0),
            ("wind", 0),
            ("esc_status", 0),
            ("event", 0),
        ] {
            w.subscribe(topic, multi);
        }
        // Roll 5°, pitch -3°, yaw 90°.
        let (roll, pitch, yaw) = (5f64.to_radians(), (-3f64).to_radians(), 90f64.to_radians());
        let (cr, sr) = ((roll / 2.0).cos(), (roll / 2.0).sin());
        let (cp, sp) = ((pitch / 2.0).cos(), (pitch / 2.0).sin());
        let (cy, sy) = ((yaw / 2.0).cos(), (yaw / 2.0).sin());
        let q = [
            cr * cp * cy + sr * sp * sy,
            sr * cp * cy - cr * sp * sy,
            cr * sp * cy + sr * cp * sy,
            cr * cp * sy - sr * sp * cy,
        ];
        for tenth in 10..=600u64 {
            let us = tenth * 100_000;
            let t = us as f64;
            let secs = t / 1e6;
            let height = if (12.0..48.0).contains(&secs) {
                20.0 * (1.0 - ((secs - 30.0) / 18.0).abs())
            } else {
                0.0
            };
            let flying = (10.0..50.0).contains(&secs);
            if tenth % 10 == 0 {
                w.data(
                    "vehicle_status",
                    0,
                    &[
                        ("timestamp", t),
                        ("arming_state", if flying { 2.0 } else { 1.0 }),
                        ("nav_state", if secs < 25.0 { 2.0 } else { 5.0 }),
                        ("vehicle_type", 1.0),
                        ("system_type", 2.0),
                    ],
                );
                w.data(
                    "vehicle_land_detected",
                    0,
                    &[
                        ("timestamp", t),
                        ("landed", f64::from(u8::from(height == 0.0))),
                    ],
                );
                for (multi, volts) in [(0, 16.4 - secs * 0.01), (1, 12.2)] {
                    w.data(
                        "battery_status",
                        multi,
                        &[
                            ("timestamp", t),
                            ("voltage_v", volts),
                            ("current_a", if flying { 12.0 } else { 0.5 }),
                            ("discharged_mah", secs * 2.0),
                            ("remaining", 0.9),
                            ("temperature", 31.5),
                            ("connected", 1.0),
                        ],
                    );
                }
                w.data(
                    "wind",
                    0,
                    &[
                        ("timestamp", t),
                        ("windspeed_north", -3.0),
                        ("windspeed_east", 0.0),
                    ],
                );
                w.data(
                    "esc_status",
                    0,
                    &[
                        ("timestamp", t),
                        ("esc_count", 2.0),
                        ("esc[0].esc_rpm", 5000.0),
                        ("esc[1].esc_rpm", 5100.0),
                        ("esc[1].esc_voltage", 16.0),
                    ],
                );
            }
            if tenth == 15 {
                w.data(
                    "home_position",
                    0,
                    &[
                        ("timestamp", t),
                        ("lat", 47.397_742),
                        ("lon", 8.545_594),
                        ("alt", 488.0),
                        ("valid_hpos", 1.0),
                    ],
                );
            }
            if tenth % 2 == 0 {
                w.data(
                    "vehicle_gps_position",
                    0,
                    &[
                        ("timestamp", t),
                        ("latitude_deg", 47.397_742 + tenth as f64 * 1e-7),
                        ("longitude_deg", 8.545_594),
                        ("altitude_msl_m", 488.0 + height),
                        ("time_utc_usec", 1_794_000_000_000_000.0 + t),
                        ("eph", 0.8),
                        ("epv", 1.2),
                        ("hdop", 0.7),
                        ("vdop", 1.1),
                        ("vel_m_s", 2.0),
                        ("cog_rad", 90f64.to_radians()),
                        ("fix_type", 3.0),
                        ("satellites_used", 12.0),
                    ],
                );
            }
            w.data(
                "vehicle_local_position",
                0,
                &[
                    ("timestamp", t),
                    ("z", -height),
                    ("vx", 2.0),
                    ("vz", 0.0),
                    ("heading", 90f64.to_radians()),
                ],
            );
            w.data(
                "vehicle_global_position",
                0,
                &[
                    ("timestamp", t),
                    ("lat", 47.397_742 + tenth as f64 * 1e-7),
                    ("lon", 8.545_594),
                    ("alt", 488.0 + height),
                    ("lat_lon_valid", 1.0),
                ],
            );
            w.data(
                "vehicle_attitude",
                0,
                &[
                    ("timestamp", t),
                    ("q[0]", q[0]),
                    ("q[1]", q[1]),
                    ("q[2]", q[2]),
                    ("q[3]", q[3]),
                ],
            );
            w.data(
                "vehicle_angular_velocity",
                0,
                &[("timestamp", t), ("xyz[2]", 0.1)],
            );
            let motor = if flying { 1500.0 } else { 900.0 };
            w.data(
                "actuator_outputs",
                0,
                &[
                    ("timestamp", t),
                    ("noutputs", 4.0),
                    ("output[0]", motor),
                    ("output[1]", motor + 10.0),
                    ("output[2]", motor + 20.0),
                    ("output[3]", motor + 30.0),
                ],
            );
            if tenth == 100 {
                w.log(b'6', us, "[commander] Armed by external command\t");
            }
            if tenth == 120 {
                w.log(b'6', us, "[commander] \u{1b}[32mTakeoff detected\u{1b}[0m");
            }
            if tenth == 200 {
                w.data(
                    "event",
                    0,
                    &[
                        ("timestamp", t),
                        ("id", 1234.0),
                        ("event_sequence", 1.0),
                        ("arguments[0]", 7.0),
                        ("log_levels", 68.0),
                    ],
                );
            }
            if tenth == 300 {
                w.param_float("MPC_XY_VEL_MAX", 8.0);
                w.log(
                    b'4',
                    us,
                    "[navigator] Returning home at 30 m above home, then landing where it took off",
                );
                w.sync();
            }
            if tenth == 400 {
                w.log(b'7', us, "[ekf2] a debug line");
            }
        }
        w.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{Writer, flight};
    use super::*;

    /// The fixture's GPS time at boot.
    const UTC_AT_BOOT: u64 = 1_794_000_000_000_000;

    fn session(tlog: &[u8]) -> Vec<crate::session::Effect> {
        let mut session = crate::session::Session::new(crate::session::Config::default());
        let mut out = Vec::new();
        for (at, frame) in crate::tlog::read(tlog) {
            out.extend(session.ingest("log", "log", frame, at));
            out.extend(session.tick(at));
        }
        out
    }

    #[test]
    fn a_log_reads_itself_from_its_formats() {
        let bytes = flight();
        assert!(is_ulog(&bytes) && !is_ulog(b"ULog"));
        let log = read(&bytes);
        assert_eq!(log.cut, 0);
        assert_eq!(log.start_us, 1_000_000);
        let attitude = &log.formats["vehicle_attitude"];
        let names: Vec<&str> = attitude.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["timestamp", "q[0]", "q[1]", "q[2]", "q[3]"]);
        // A nested topic, flattened after the padding before it.
        let esc = &log.formats["esc_status"];
        let rpm = esc
            .fields
            .iter()
            .find(|f| f.name == "esc[1].esc_rpm")
            .unwrap();
        assert_eq!(rpm.at, 8 + 8 + 16 + 8);
        assert!(esc.fields.iter().all(|f| !f.name.contains("_padding")));
        assert_eq!(
            log.info.get("sys_name").and_then(Value::as_text),
            Some("PX4")
        );
        assert_eq!(
            log.parameters.len(),
            4,
            "a change is not a parameter of its own"
        );
        assert_eq!(
            log.defaults.get("MPC_XY_VEL_MAX"),
            Some(&10.0),
            "its airframe's"
        );
        assert_eq!(log.defaults.get("COM_RC_IN_MODE"), Some(&0.0));
        // PX4's own default differs, but its airframe set it as it is.
        assert_eq!(log.defaults.get("MAV_TYPE"), Some(&2.0));
        assert_eq!(log.defaults.get("MAV_SYS_ID"), Some(&7.0), "as it is");
        let rpm = series(&bytes, &["df.esc_status.esc[1].esc_rpm".to_owned()]);
        assert!(
            rpm["df.esc_status.esc[1].esc_rpm"]
                .iter()
                .all(|(_, v)| (*v - 5100.0).abs() < 1e-9)
        );
    }

    #[test]
    fn the_producer_gives_px4s_messages_on_gps_time() {
        let bytes = flight();
        let (tlog, about) = to_tlog(&bytes).unwrap();
        assert_eq!(about.system_id, 7);
        assert!(about.gps_time && about.mission.is_empty());
        assert_eq!(about.firmware.as_deref(), Some("PX4 v1.15.0 (01234567)"));
        assert_eq!(about.defaults.get("MPC_XY_VEL_MAX"), Some(&10.0));
        let records = crate::tlog::read(&tlog);
        assert!(records.windows(2).all(|w| w[0].0 <= w[1].0), "in order");
        // Logging began 1 s after boot, on GPS time.
        assert_eq!(records[0].0, UTC_AT_BOOT + 1_000_000);
        let mut seen = Vec::new();
        let mut keys = BTreeSet::new();
        let mut parameters: BTreeMap<String, Vec<f32>> = BTreeMap::new();
        let mut texts = Vec::new();
        let mut commit = None;
        for effect in session(&tlog) {
            match effect {
                crate::session::Effect::Vehicle(vehicle) => {
                    assert_eq!(vehicle.system_id, 7);
                    assert!(vehicle.autopilot.eq_ignore_ascii_case("px4"));
                    commit.clone_from(&vehicle.firmware_commit);
                    let state = (vehicle.armed, vehicle.mode.clone());
                    if seen.last() != Some(&state) {
                        seen.push(state);
                    }
                }
                crate::session::Effect::Telemetry(t) => {
                    keys.extend(t.items.into_iter().map(|i| i.key));
                }
                crate::session::Effect::ParamValue { param, .. } => {
                    parameters.entry(param.name).or_default().push(param.value);
                }
                crate::session::Effect::StatusText { text, .. } => texts.push(text),
                _ => {}
            }
        }
        assert!(seen.contains(&(true, Some("Position".into()))), "{seen:?}");
        assert!(seen.contains(&(true, Some("Return".into()))), "{seen:?}");
        assert_eq!(seen.last(), Some(&(false, Some("Return".into()))));
        assert_eq!(commit.as_deref(), Some("01234567"));
        assert_eq!(
            parameters["MAV_SYS_ID"],
            [7.0],
            "an integer's bits as its number"
        );
        assert_eq!(parameters["COM_RC_IN_MODE"], [3.0]);
        assert_eq!(
            parameters["MPC_XY_VEL_MAX"],
            [12.0, 8.0],
            "and its change in flight"
        );
        for key in [
            "home.latitude_deg",
            "servo.1",
            "servo.4",
            "battery_2.temperature_degc",
            "attitude.roll_deg",
        ] {
            assert!(keys.contains(key), "{key} in {keys:?}");
        }
        assert!(!keys.contains("servo.5"), "an unused output stays out");
        assert!(
            texts.contains(&"Armed by external command".to_owned()),
            "{texts:?}"
        );
        assert!(texts.contains(&"Takeoff detected".to_owned()), "{texts:?}");
        assert!(
            texts.contains(
                &"Returning home at 30 m above home, then landing where it took off".to_owned()
            ),
            "a long one in parts: {texts:?}"
        );
        assert!(!texts.iter().any(|t| t.contains("debug")), "{texts:?}");
    }

    #[test]
    fn its_position_flight_state_and_events_are_px4s() {
        let (tlog, _) = to_tlog(&flight()).unwrap();
        let mut decoded = Vec::new();
        for (at, frame) in crate::tlog::read(&tlog) {
            if let Some(d) = crate::decode::decode(frame) {
                decoded.push((at, d.message));
            }
        }
        let at = |s: u64| UTC_AT_BOOT + s * 1_000_000;
        let above_home = decoded.iter().find_map(|(t, m)| match m {
            crate::decode::Message::GlobalPositionInt { relative_alt, .. } if *t >= at(30) => {
                Some(*relative_alt)
            }
            _ => None,
        });
        assert_eq!(above_home, Some(20_000), "20 m above home at 30 s");
        let landed: Vec<(u64, u8)> = decoded
            .iter()
            .filter_map(|(t, m)| match m {
                crate::decode::Message::ExtendedSysState { landed_state, .. } => {
                    Some((*t, *landed_state))
                }
                _ => None,
            })
            .collect();
        assert!(
            landed
                .iter()
                .any(|(t, s)| *t > at(20) && *t < at(40) && *s == 2)
        );
        assert_eq!(landed.last().map(|l| l.1), Some(1));
        assert!(decoded.iter().any(|(t, m)| *t == at(20)
            && matches!(
                m,
                crate::decode::Message::Event {
                    id: 1234,
                    sequence: 1,
                    ..
                }
            )));
    }

    #[test]
    fn a_log_without_gps_counts_from_boot() {
        let mut w = Writer::new(2_000_000);
        w.format(
            "vehicle_status",
            "uint64_t timestamp;uint8_t arming_state;uint8_t nav_state;",
        );
        w.subscribe("vehicle_status", 0);
        w.data(
            "vehicle_status",
            0,
            &[("timestamp", 5_000_000.0), ("nav_state", 4.0)],
        );
        let (tlog, about) = to_tlog(&w.bytes).unwrap();
        assert!(!about.gps_time);
        assert_eq!(about.system_id, 1);
        let records = crate::tlog::read(&tlog);
        assert_eq!(records[0].0, 2_000_000);
        assert!(!crate::tlog::sane(records[0].0));
        assert!(to_tlog(&[0u8; 100]).is_err());
        assert!(to_tlog(&Writer::new(0).bytes).is_err(), "no topics");
    }

    #[test]
    fn a_log_cut_short_or_corrupt_plays_what_it_can() {
        let bytes = flight();
        let whole = crate::tlog::read(&to_tlog(&bytes).unwrap().0).len();
        let cut = &bytes[..bytes.len() - 7];
        assert!(read(cut).cut > 0);
        let records = crate::tlog::read(&to_tlog(cut).unwrap().0).len();
        assert!(records > 0 && records <= whole);
        // Noise in the middle: read again from the next sync message.
        let half = bytes.len() / 3;
        let mut corrupt = bytes[..half].to_vec();
        corrupt.extend_from_slice(&[0xEE; 40]);
        corrupt.extend_from_slice(&bytes[half..]);
        let key = "df.vehicle_gps_position.latitude_deg".to_owned();
        let gps = series(&corrupt, std::slice::from_ref(&key));
        assert_eq!(
            gps[&key].last().unwrap().0,
            UTC_AT_BOOT + 60_000_000,
            "the end is read"
        );
    }

    #[test]
    fn every_topic_is_listed_with_its_instances_units_and_rate() {
        let messages = messages(&flight());
        let names: Vec<&str> = messages.iter().map(|m| m.name.as_str()).collect();
        assert!(
            names.contains(&"vehicle_attitude") && names.contains(&"event"),
            "{names:?}"
        );
        let battery = messages
            .iter()
            .find(|m| m.name == "battery_status")
            .unwrap();
        assert_eq!(battery.instances, [0, 1]);
        assert!(
            (battery.rate_hz - 2.0).abs() < 0.1,
            "two at 1 Hz: {}",
            battery.rate_hz
        );
        let volts = battery
            .columns
            .iter()
            .find(|c| c.name == "voltage_v")
            .unwrap();
        assert_eq!(volts.unit.as_deref(), Some("V"));
        assert!(battery.columns.iter().all(|c| c.name != "timestamp"));
        let gps = messages
            .iter()
            .find(|m| m.name == "vehicle_gps_position")
            .unwrap();
        assert!(gps.instances.is_empty());
        let speed = gps.columns.iter().find(|c| c.name == "vel_m_s").unwrap();
        assert_eq!(speed.unit.as_deref(), Some("m/s"));
        let attitude = messages
            .iter()
            .find(|m| m.name == "vehicle_attitude")
            .unwrap();
        assert!((attitude.rate_hz - 10.0).abs() < 0.1);
    }

    #[test]
    fn a_series_is_as_logged_at_its_own_rate_on_the_replays_clock() {
        let bytes = flight();
        let keys: Vec<String> = [
            "df.battery_status[1].voltage_v",
            "df.battery_status.voltage_v",
            "df.actuator_outputs.output[0]",
            "df.vehicle_gps_position.latitude_deg",
            "df.vehicle_gps_position.nope",
            "df.nope.x",
            "battery.voltage_v",
        ]
        .map(String::from)
        .to_vec();
        let series = series(&bytes, &keys);
        assert!(
            series["df.battery_status[1].voltage_v"]
                .iter()
                .all(|(_, v)| (*v - 12.2).abs() < 1e-5)
        );
        assert!((series["df.battery_status.voltage_v"][0].1 - 16.39).abs() < 1e-5);
        // Every tenth of a second, from 1 s to 60 s: no thinning.
        assert_eq!(series["df.actuator_outputs.output[0]"].len(), 591);
        let gps = &series["df.vehicle_gps_position.latitude_deg"];
        assert!((gps[0].1 - 47.397_743).abs() < 1e-9);
        assert_eq!(gps[0].0, UTC_AT_BOOT + 1_000_000, "on GPS time");
        assert_eq!(series.len(), 4, "{:?}", series.keys());
    }

    #[test]
    fn its_modes_are_px4s() {
        for (nav_state, name) in [
            (0, "Manual"),
            (1, "Altitude"),
            (2, "Position"),
            (3, "Mission"),
            (4, "Hold"),
            (5, "Return"),
            (10, "Acro"),
            (14, "Offboard"),
            (15, "Stabilized"),
            (17, "Takeoff"),
            (18, "Land"),
            (19, "Follow Me"),
            (20, "Precision Land"),
            (21, "Orbit"),
            (22, "VTOL Takeoff"),
        ] {
            assert_eq!(
                crate::modes::Family::Px4.name(custom_mode(nav_state)),
                Some(name),
                "{nav_state}"
            );
        }
    }

    #[test]
    fn a_long_message_ends_in_parts() {
        assert_eq!(parts("short"), ["short"]);
        let fifty = "x".repeat(50);
        assert_eq!(parts(&fifty), [fifty.as_str()]);
        let hundred = "y".repeat(100);
        assert_eq!(parts(&hundred), [&hundred[..50], &hundred[50..], ""]);
        assert_eq!(parts(&"z".repeat(1000)).len(), 8);
        assert_eq!(
            plain("[commander] \u{1b}[32mReady for takeoff!\u{1b}[0m\t"),
            "Ready for takeoff!"
        );
        assert_eq!(plain("[a b] kept"), "[a b] kept");
    }
}

/// A real PX4 log, not kept in the repository: `UXDUCK_ULG=<path> cargo
/// test -p uxduck-mavlink --lib ulog -- --ignored --nocapture`.
#[cfg(test)]
#[test]
#[ignore = "needs a log on this computer"]
fn a_real_px4_log_plays() {
    let Ok(path) = std::env::var("UXDUCK_ULG") else {
        return;
    };
    let bytes = std::fs::read(path).unwrap_or_default();
    let started = std::time::Instant::now();
    let (tlog, about) = to_tlog(&bytes).unwrap_or_default();
    let records = crate::tlog::read(&tlog);
    let converted = started.elapsed();
    let fields = messages(&bytes);
    println!(
        "{} MB read, {} records out in {converted:?}, {} topics listed in {:?}, system {}, {:?}, GPS time {}, {} defaults, first {:?}",
        bytes.len() >> 20,
        records.len(),
        fields.len(),
        started.elapsed().saturating_sub(converted),
        about.system_id,
        about.firmware,
        about.gps_time,
        about.defaults.len(),
        records.first().map(|r| r.0)
    );
    let mut session = crate::session::Session::new(crate::session::Config::default());
    let mut seen = Vec::new();
    let mut keys = BTreeSet::new();
    for (at, frame) in &records {
        let mut effects = session.ingest("log", "log", frame, *at);
        effects.extend(session.tick(*at));
        for effect in effects {
            match effect {
                crate::session::Effect::Vehicle(v) => {
                    let state = (v.armed, v.mode.clone(), v.vehicle_type.clone());
                    if seen.last() != Some(&state) {
                        println!("{at}: {state:?}");
                        seen.push(state);
                    }
                }
                crate::session::Effect::StatusText { text, .. } => println!("{at}: {text}"),
                crate::session::Effect::Telemetry(t) => {
                    keys.extend(t.items.into_iter().map(|i| i.key));
                }
                _ => {}
            }
        }
    }
    println!("{} telemetry keys: {keys:?}", keys.len());
    assert!(!records.is_empty());
}

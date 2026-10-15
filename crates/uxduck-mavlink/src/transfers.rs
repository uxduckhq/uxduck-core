//! The parameter and mission protocols as state machines (ADR-0004). Each
//! takes what the vehicle said and the time, and says what to send next;
//! `Session` addresses and frames it. No I/O here.
//!
//! Parameters come by MAVLink FTP first (S74): ArduPilot's packed file
//! `@PARAM/param.pck?withdefaults=1` holds every value with its default.
//! Without FTP, or that file, they come by the full list
//! (`PARAM_REQUEST_LIST`) and then the gaps one by one
//! (`PARAM_REQUEST_READ`), which every autopilot supports, without
//! defaults.

use std::collections::BTreeMap;

use crate::decode::MissionItemInt;

/// One parameter as the autopilot reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub name: String,
    /// The numeric value, as ArduPilot sends every type in a float
    /// (`MAV_PROTOCOL_CAPABILITY_PARAM_ENCODE_C_CAST`). PX4 sends an
    /// integer's bits instead; [`from_bytewise`] turns those into this.
    pub value: f32,
    /// `MAV_PARAM_TYPE`.
    pub kind: u8,
}

/// Something to send to the vehicle; `Session` frames it.
#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    ParamRequestList,
    ParamRequestRead(u16),
    ParamSet(Param),
    MissionRequestList,
    MissionCount(u16),
    MissionRequest(u16),
    MissionItem(MissionItemInt),
    MissionAck(u8),
    /// A MAVLink FTP request (`FILE_TRANSFER_PROTOCOL`'s payload).
    Ftp(FtpRequest),
}

/// Why a transfer did not go through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// Never sent: no address, a read-only link, another transfer running.
    NotSent(String),
    /// The vehicle said no, in its words.
    Rejected(String),
    /// No answer after every attempt.
    Timeout,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSent(why) | Self::Rejected(why) => f.write_str(why),
            Self::Timeout => f.write_str("the vehicle did not answer"),
        }
    }
}

/// How patient each protocol is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    /// Silence during a parameter list before asking for the gaps.
    pub param_gap_us: u64,
    /// Rounds of asking without anything new before giving up.
    pub param_rounds: u8,
    /// How long to wait for each answer, and how many times to send.
    pub timeout_us: u64,
    pub attempts: u8,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            param_gap_us: 1_000_000,
            param_rounds: 5,
            timeout_us: 1_500_000,
            attempts: 3,
        }
    }
}

/// How many missing parameters to ask for in one round.
const READS_PER_ROUND: usize = 50;

/// What a tick decided.
#[derive(Debug, PartialEq)]
pub enum Step<T> {
    /// Keep going; send these.
    Send(Vec<Out>),
    Done(T),
    Failed(Failure),
}

/// Downloading the whole parameter list.
#[derive(Clone, Debug)]
pub struct ParamDownload {
    by_index: BTreeMap<u16, Param>,
    count: Option<u16>,
    last_progress_us: u64,
    rounds: u8,
}

impl ParamDownload {
    #[must_use]
    pub fn start(now_us: u64) -> (Self, Vec<Out>) {
        (
            Self {
                by_index: BTreeMap::new(),
                count: None,
                last_progress_us: now_us,
                rounds: 0,
            },
            vec![Out::ParamRequestList],
        )
    }

    /// How far along it is: `(received, count)`.
    #[must_use]
    pub fn progress(&self) -> (usize, Option<u16>) {
        (self.by_index.len(), self.count)
    }

    /// A `PARAM_VALUE` from the vehicle. `Some` with the whole list once
    /// every index has arrived.
    pub fn value(
        &mut self,
        param: Param,
        count: u16,
        index: u16,
        now_us: u64,
    ) -> Option<Vec<Param>> {
        if index == u16::MAX || index >= count {
            return None;
        }
        self.count = Some(count);
        if self.by_index.insert(index, param).is_none() {
            self.last_progress_us = now_us;
            self.rounds = 0;
        }
        (self.by_index.len() == usize::from(count))
            .then(|| std::mem::take(&mut self.by_index).into_values().collect())
    }

    /// Asks again after a silence: the whole list if nothing has come, the
    /// missing ones otherwise; gives up after a few rounds with nothing new.
    pub fn tick(&mut self, now_us: u64, timing: &Timing) -> Step<Vec<Param>> {
        if now_us.saturating_sub(self.last_progress_us) < timing.param_gap_us {
            return Step::Send(Vec::new());
        }
        if self.rounds >= timing.param_rounds {
            let (received, count) = self.progress();
            return Step::Failed(match count {
                None => Failure::Timeout,
                Some(count) => {
                    Failure::Rejected(format!("received {received} of {count} parameters"))
                }
            });
        }
        self.rounds += 1;
        self.last_progress_us = now_us;
        let Some(count) = self.count else {
            return Step::Send(vec![Out::ParamRequestList]);
        };
        Step::Send(
            (0..count)
                .filter(|index| !self.by_index.contains_key(index))
                .take(READS_PER_ROUND)
                .map(Out::ParamRequestRead)
                .collect(),
        )
    }
}

/// Setting one parameter: sent until its `PARAM_VALUE` comes back.
#[derive(Clone, Debug)]
pub struct ParamWrite {
    pub order_id: String,
    pub param: Param,
    attempts: u8,
    sent_us: u64,
}

impl ParamWrite {
    #[must_use]
    pub fn start(order_id: String, param: Param, now_us: u64) -> (Self, Vec<Out>) {
        let out = vec![Out::ParamSet(param.clone())];
        (
            Self {
                order_id,
                param,
                attempts: 1,
                sent_us: now_us,
            },
            out,
        )
    }

    #[must_use]
    pub const fn attempts(&self) -> u8 {
        self.attempts
    }

    pub fn tick(&mut self, now_us: u64, timing: &Timing) -> Step<()> {
        if now_us.saturating_sub(self.sent_us) < timing.timeout_us {
            return Step::Send(Vec::new());
        }
        if self.attempts >= timing.attempts {
            return Step::Failed(Failure::Timeout);
        }
        self.attempts += 1;
        self.sent_us = now_us;
        Step::Send(vec![Out::ParamSet(self.param.clone())])
    }
}

/// A MAVLink FTP request: the payload's header and data (S74).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FtpRequest {
    pub seq: u16,
    pub session: u8,
    pub opcode: u8,
    /// The payload's `size`: the data's length, except for a read, where
    /// it is how many bytes to read and the data is empty.
    pub size: u8,
    pub offset: u32,
    pub data: Vec<u8>,
}

/// MAVLink FTP's opcodes and errors (S74, S121).
pub mod ftp {
    pub const TERMINATE_SESSION: u8 = 1;
    pub const RESET_SESSIONS: u8 = 2;
    pub const LIST_DIRECTORY: u8 = 3;
    pub const OPEN_FILE_RO: u8 = 4;
    pub const READ_FILE: u8 = 5;
    pub const CREATE_FILE: u8 = 6;
    pub const WRITE_FILE: u8 = 7;
    pub const REMOVE_FILE: u8 = 8;
    pub const CREATE_DIRECTORY: u8 = 9;
    pub const REMOVE_DIRECTORY: u8 = 10;
    pub const OPEN_FILE_WO: u8 = 11;
    pub const TRUNCATE_FILE: u8 = 12;
    pub const RENAME: u8 = 13;
    pub const CALC_FILE_CRC32: u8 = 14;
    pub const BURST_READ_FILE: u8 = 15;
    pub const ACK: u8 = 128;
    pub const NAK: u8 = 129;
    /// `NAK` error: unknown failure.
    pub const FAIL: u8 = 1;
    /// `NAK` error: the vehicle's file system said no (its `errno` follows).
    pub const FAIL_ERRNO: u8 = 2;
    pub const INVALID_DATA_SIZE: u8 = 3;
    pub const INVALID_SESSION: u8 = 4;
    pub const NO_SESSIONS_AVAILABLE: u8 = 5;
    /// `NAK` error: past the end of the file, or of a listing.
    pub const EOF: u8 = 6;
    pub const UNKNOWN_COMMAND: u8 = 7;
    pub const FILE_EXISTS: u8 = 8;
    pub const FILE_PROTECTED: u8 = 9;
    pub const FILE_NOT_FOUND: u8 = 10;
    /// Data bytes in one payload.
    pub const DATA_MAX: usize = 239;
    /// ArduPilot's packed parameters, with their defaults.
    pub const PARAM_PATH: &str = "@PARAM/param.pck?withdefaults=1";
    /// Where ArduPilot takes packed parameters to set them.
    pub const PARAM_UPLOAD_PATH: &str = "@PARAM/param.pck";

    /// A `NAK`'s error in words: `data[0]` the error, `data[1]` the
    /// `errno` when the file system said no.
    #[must_use]
    pub fn error_text(data: &[u8]) -> String {
        match data.first().copied().unwrap_or(FAIL) {
            FAIL_ERRNO => match data.get(1).copied().unwrap_or(0) {
                2 => "there is no such file or folder".into(),
                5 => "the vehicle's storage could not be read or written".into(),
                13 | 1 => "the vehicle does not allow that".into(),
                17 => "something with that name is already there".into(),
                20 => "that is not a folder".into(),
                21 => "that is a folder".into(),
                22 => "the vehicle refused the request".into(),
                28 => "the vehicle's storage is full".into(),
                30 => "the vehicle's storage is read-only".into(),
                39 => "the folder is not empty".into(),
                errno => format!("the vehicle's file system refused it (error {errno})"),
            },
            INVALID_DATA_SIZE => "the vehicle refused the request's size".into(),
            INVALID_SESSION => "the vehicle lost track of the transfer".into(),
            NO_SESSIONS_AVAILABLE => {
                "another ground station is using the vehicle's file transfer".into()
            }
            EOF => "past the end of the file".into(),
            UNKNOWN_COMMAND => "this vehicle does not do that over file transfer".into(),
            FILE_EXISTS => "something with that name is already there".into(),
            FILE_PROTECTED => "the vehicle protects that file".into(),
            FILE_NOT_FOUND => "there is no such file or folder".into(),
            _ => "the vehicle could not do it".into(),
        }
    }

    /// A request as `FILE_TRANSFER_PROTOCOL`'s 251-byte payload.
    #[must_use]
    pub fn payload(request: &super::FtpRequest) -> [u8; 251] {
        let mut out = [0u8; 251];
        out[0..2].copy_from_slice(&request.seq.to_le_bytes());
        out[2] = request.session;
        out[3] = request.opcode;
        out[4] = request.size;
        out[8..12].copy_from_slice(&request.offset.to_le_bytes());
        let len = request.data.len().min(DATA_MAX);
        out[12..12 + len].copy_from_slice(&request.data[..len]);
        out
    }

    /// An answer, read from its payload.
    #[must_use]
    pub fn read(payload: &[u8; 251]) -> Answer {
        let size = payload[4];
        let len = usize::from(size).min(DATA_MAX);
        Answer {
            seq: u16::from_le_bytes([payload[0], payload[1]]),
            session: payload[2],
            opcode: payload[3],
            size,
            req_opcode: payload[5],
            burst_complete: payload[6] != 0,
            offset: u32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]]),
            data: payload[12..12 + len].to_vec(),
        }
    }

    /// A decoded FTP payload.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Answer {
        pub seq: u16,
        pub session: u8,
        pub opcode: u8,
        /// The `size` field: the data's length, or what a read asks for.
        pub size: u8,
        pub req_opcode: u8,
        /// The last answer of a burst read.
        pub burst_complete: bool,
        pub offset: u32,
        pub data: Vec<u8>,
    }
}

/// A parameter with its default, as `param.pck` has them.
pub type WithDefault = (Param, Option<f32>);

/// Reads ArduPilot's `param.pck`: a header (magic `0x671b`, or `0x671c`
/// with defaults; the count; the total), then for each parameter a byte of
/// type and flags, a byte of the name's shared prefix and its new length,
/// the new part of the name, the value and, when flagged, the default.
/// Zero bytes between entries are padding. In a file with defaults
/// (`0x671c`), ArduPilot flags a default only where it differs: an
/// unflagged parameter is at its default.
///
/// # Errors
/// When the file is not a parameter pack or ends inside an entry.
pub fn parse_param_pck(bytes: &[u8]) -> Result<Vec<WithDefault>, String> {
    if bytes.len() < 6 {
        return Err("the parameter file is too short".into());
    }
    let magic = u16::from_le_bytes([bytes[0], bytes[1]]);
    if magic != 0x671b && magic != 0x671c {
        return Err(format!("not a parameter file (magic {magic:#06x})"));
    }
    let with_defaults = magic == 0x671c;
    let count = usize::from(u16::from_le_bytes([bytes[2], bytes[3]]));
    let mut out = Vec::with_capacity(count);
    let mut name = String::new();
    let mut at = 6;
    while out.len() < count {
        let Some(&first) = bytes.get(at) else {
            return Err(format!(
                "the file ends after {} of {count} parameters",
                out.len()
            ));
        };
        if first == 0 {
            at += 1;
            continue;
        }
        let ap_type = first & 0x0f;
        let flags = first >> 4;
        let second = *bytes.get(at + 1).ok_or("the file ends inside a name")?;
        let common = usize::from(second & 0x0f);
        let fresh = usize::from(second >> 4) + 1;
        at += 2;
        let part = bytes
            .get(at..at + fresh)
            .ok_or("the file ends inside a name")?;
        name.truncate(common.min(name.len()));
        name.push_str(&String::from_utf8_lossy(part));
        at += fresh;
        let (size, kind) = match ap_type {
            1 => (1, 2u8),
            2 => (2, 4),
            3 => (4, 6),
            4 => (4, 9),
            other => return Err(format!("unknown parameter type {other} for {name}")),
        };
        let value_at = |at: usize| -> Result<f32, String> {
            let raw = bytes
                .get(at..at + size)
                .ok_or("the file ends inside a value")?;
            #[allow(clippy::cast_precision_loss, reason = "parameters are f32 on the wire")]
            Ok(match ap_type {
                1 => f32::from(i8::from_le_bytes([raw[0]])),
                2 => f32::from(i16::from_le_bytes([raw[0], raw[1]])),
                3 => i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as f32,
                _ => f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]),
            })
        };
        let value = value_at(at)?;
        at += size;
        let default = if flags & 1 == 1 {
            let default = value_at(at)?;
            at += size;
            Some(default)
        } else {
            with_defaults.then_some(value)
        };
        out.push((
            Param {
                name: name.clone(),
                value,
                kind,
            },
            default,
        ));
    }
    Ok(out)
}

/// Writes a `param.pck` with defaults, sorted by name, for the fake vehicle
/// and tests: as ArduPilot does, a default only where it differs from the
/// value. Entries never cross a 512-byte block: the rest is padding.
#[must_use]
pub fn encode_param_pck(params: &[WithDefault]) -> Vec<u8> {
    let mut sorted: Vec<&WithDefault> = params.iter().collect();
    sorted.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    let count = u16::try_from(sorted.len()).unwrap_or(u16::MAX);
    let mut out = Vec::new();
    out.extend_from_slice(&0x671c_u16.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    let mut previous = "";
    for (param, default) in sorted {
        let name = param.name.as_str();
        let common = previous
            .bytes()
            .zip(name.bytes())
            .take_while(|(a, b)| a == b)
            .count()
            .min(15)
            .min(name.len() - 1);
        let fresh = &name.as_bytes()[common..];
        let (ap_type, size) = match param.kind {
            1 | 2 => (1u8, 1),
            3 | 4 => (2, 2),
            5 | 6 => (3, 4),
            _ => (4, 4),
        };
        let value = |v: f32| -> Vec<u8> {
            #[allow(clippy::cast_possible_truncation, reason = "integer parameters")]
            match ap_type {
                1 => (v as i8).to_le_bytes().to_vec(),
                2 => (v as i16).to_le_bytes().to_vec(),
                3 => (v as i32).to_le_bytes().to_vec(),
                _ => v.to_le_bytes().to_vec(),
            }
        };
        #[allow(clippy::float_cmp, reason = "ArduPilot compares the stored values")]
        let default = default.filter(|d| *d != param.value);
        let default = default.as_ref();
        let mut entry = vec![
            ap_type | (u8::from(default.is_some()) << 4),
            u8::try_from(common).unwrap_or(0) | (u8::try_from(fresh.len() - 1).unwrap_or(0) << 4),
        ];
        entry.extend_from_slice(fresh);
        entry.extend(value(param.value));
        if let Some(default) = default {
            entry.extend(value(*default));
        }
        let _ = size;
        let left = 512 - out.len() % 512;
        if entry.len() > left {
            out.resize(out.len() + left, 0);
        }
        out.extend(entry);
        previous = name;
    }
    out
}

/// Writes a `param.pck` that ArduPilot takes to set parameters (S121,
/// ADR-0027 F13): magic `0x671b`, no defaults, no padding, the header's
/// total the file's length, names sorted with their shared prefixes. Each
/// entry's prefix is at most the previous name's length and a name at most
/// 16 characters, as ArduPilot checks.
#[must_use]
pub fn encode_param_upload(params: &[Param]) -> Vec<u8> {
    let mut sorted: Vec<&Param> = params.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    sorted.dedup_by(|a, b| a.name == b.name);
    let count = u16::try_from(sorted.len()).unwrap_or(u16::MAX);
    let mut out = Vec::new();
    out.extend_from_slice(&0x671b_u16.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes());
    let mut previous = "";
    for param in sorted {
        let name = &param.name[..param.name.len().min(16)];
        let common = previous
            .bytes()
            .zip(name.bytes())
            .take_while(|(a, b)| a == b)
            .count()
            .min(15)
            .min(name.len().saturating_sub(1));
        let fresh = &name.as_bytes()[common..];
        #[allow(clippy::cast_possible_truncation, reason = "integer parameters")]
        let (ap_type, value): (u8, Vec<u8>) = match param.kind {
            1 | 2 => (1, (param.value as i8).to_le_bytes().to_vec()),
            3 | 4 => (2, (param.value as i16).to_le_bytes().to_vec()),
            5 | 6 => (3, (param.value as i32).to_le_bytes().to_vec()),
            _ => (4, param.value.to_le_bytes().to_vec()),
        };
        out.push(ap_type);
        out.push(
            u8::try_from(common).unwrap_or(0)
                | (u8::try_from(fresh.len().saturating_sub(1)).unwrap_or(0) << 4),
        );
        out.extend_from_slice(fresh);
        out.extend(value);
        previous = name;
    }
    let total = u16::try_from(out.len()).unwrap_or(u16::MAX);
    out[4..6].copy_from_slice(&total.to_le_bytes());
    out
}

/// `MAV_MISSION_RESULT` in words.
#[must_use]
pub fn mission_result_text(result: u8) -> &'static str {
    match result {
        0 => "Accepted",
        2 => "Unsupported frame",
        3 => "Unsupported command",
        4 => "No space for that many items",
        5 => "Invalid item",
        6..=12 => "An item's value is out of range",
        13 => "Items out of order",
        14 => "Denied",
        15 => "Operation cancelled",
        _ => "Error",
    }
}

/// Where a message exchange stands: what was last sent and when.
#[derive(Clone, Debug)]
struct Exchange {
    last: Vec<Out>,
    attempts: u8,
    sent_us: u64,
}

impl Exchange {
    fn new(out: Vec<Out>, now_us: u64) -> Self {
        Self {
            last: out,
            attempts: 1,
            sent_us: now_us,
        }
    }

    fn sent(&mut self, out: &[Out], now_us: u64) {
        self.last = out.to_vec();
        self.attempts = 1;
        self.sent_us = now_us;
    }

    /// Sends the last message again after a timeout, until the attempts
    /// run out.
    fn tick<T>(&mut self, now_us: u64, timing: &Timing) -> Step<T> {
        if now_us.saturating_sub(self.sent_us) < timing.timeout_us {
            return Step::Send(Vec::new());
        }
        if self.attempts >= timing.attempts {
            return Step::Failed(Failure::Timeout);
        }
        self.attempts += 1;
        self.sent_us = now_us;
        Step::Send(self.last.clone())
    }
}

/// Downloading the mission: the count, then each item in turn.
#[derive(Clone, Debug)]
pub struct MissionDownload {
    count: Option<u16>,
    items: Vec<MissionItemInt>,
    exchange: Exchange,
}

impl MissionDownload {
    #[must_use]
    pub fn start(now_us: u64) -> (Self, Vec<Out>) {
        let out = vec![Out::MissionRequestList];
        (
            Self {
                count: None,
                items: Vec::new(),
                exchange: Exchange::new(out.clone(), now_us),
            },
            out,
        )
    }

    pub fn count(&mut self, count: u16, now_us: u64) -> Step<Vec<MissionItemInt>> {
        if self.count.is_some() {
            return Step::Send(Vec::new());
        }
        self.count = Some(count);
        if count == 0 {
            return Step::Done(Vec::new());
        }
        let out = vec![Out::MissionRequest(0)];
        self.exchange.sent(&out, now_us);
        Step::Send(out)
    }

    pub fn item(&mut self, item: MissionItemInt, now_us: u64) -> Step<Vec<MissionItemInt>> {
        let Some(count) = self.count else {
            return Step::Send(Vec::new());
        };
        if usize::from(item.seq) != self.items.len() {
            return Step::Send(Vec::new());
        }
        self.items.push(item);
        if self.items.len() == usize::from(count) {
            return Step::Done(std::mem::take(&mut self.items));
        }
        let next = u16::try_from(self.items.len()).unwrap_or(u16::MAX);
        let out = vec![Out::MissionRequest(next)];
        self.exchange.sent(&out, now_us);
        Step::Send(out)
    }

    pub fn tick(&mut self, now_us: u64, timing: &Timing) -> Step<Vec<MissionItemInt>> {
        self.exchange.tick(now_us, timing)
    }
}

/// Uploading a mission: the count, then each item the vehicle asks for,
/// then its acknowledgement.
#[derive(Clone, Debug)]
pub struct MissionUpload {
    pub order_id: String,
    items: Vec<MissionItemInt>,
    exchange: Exchange,
}

impl MissionUpload {
    #[must_use]
    pub fn start(order_id: String, items: Vec<MissionItemInt>, now_us: u64) -> (Self, Vec<Out>) {
        let count = u16::try_from(items.len()).unwrap_or(u16::MAX);
        let out = vec![Out::MissionCount(count)];
        (
            Self {
                order_id,
                items,
                exchange: Exchange::new(out.clone(), now_us),
            },
            out,
        )
    }

    pub fn request(&mut self, seq: u16, now_us: u64) -> Step<()> {
        let Some(item) = self.items.get(usize::from(seq)) else {
            return Step::Send(Vec::new());
        };
        let mut item = item.clone();
        item.seq = seq;
        let out = vec![Out::MissionItem(item)];
        self.exchange.sent(&out, now_us);
        Step::Send(out)
    }

    #[must_use]
    pub fn ack(&self, result: u8) -> Step<()> {
        if result == 0 {
            Step::Done(())
        } else {
            Step::Failed(Failure::Rejected(mission_result_text(result).to_owned()))
        }
    }

    pub fn tick(&mut self, now_us: u64, timing: &Timing) -> Step<()> {
        self.exchange.tick(now_us, timing)
    }
}

/// A value PX4 sent: an integer type's bits in the float
/// (`MAV_PROTOCOL_CAPABILITY_PARAM_ENCODE_BYTEWISE`), as its number. A
/// float stays as it is.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    reason = "each MAV_PARAM_TYPE's own width"
)]
pub fn from_bytewise(value: f32, kind: u8) -> f32 {
    let bits = value.to_bits();
    match kind {
        1 => f32::from(bits as u8),
        2 => f32::from(bits as u8 as i8),
        3 => f32::from(bits as u16),
        4 => f32::from(bits as u16 as i16),
        5 => bits as f32,
        6 => bits as i32 as f32,
        _ => value,
    }
}

/// A number as PX4 takes it: an integer type's bits in the float.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "each MAV_PARAM_TYPE's own width"
)]
pub fn to_bytewise(value: f32, kind: u8) -> f32 {
    let bits = match kind {
        1 => u32::from(value as u8),
        2 => u32::from(value as i8 as u8),
        3 => u32::from(value as u16),
        4 => u32::from(value as i16 as u16),
        5 => value as u32,
        6 => value as i32 as u32,
        _ => return value,
    };
    f32::from_bits(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn param(name: &str) -> Param {
        Param {
            name: name.into(),
            value: 1.0,
            kind: 9,
        }
    }

    #[test]
    fn a_parameter_list_with_gaps_is_filled_by_index_or_given_up() {
        let timing = Timing::default();
        let (mut download, out) = ParamDownload::start(0);
        assert_eq!(out, [Out::ParamRequestList]);
        assert!(download.value(param("A"), 3, 0, 10).is_none());
        assert!(download.value(param("C"), 3, 2, 20).is_none());
        assert_eq!(download.tick(500_000, &timing), Step::Send(vec![]));
        assert_eq!(
            download.tick(1_100_000, &timing),
            Step::Send(vec![Out::ParamRequestRead(1)])
        );
        let all = download.value(param("B"), 3, 1, 1_200_000).unwrap();
        let names: Vec<&str> = all.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["A", "B", "C"], "in index order");

        let (mut silent, _) = ParamDownload::start(0);
        let mut steps = Vec::new();
        for s in 1..=7u64 {
            steps.push(silent.tick(s * 1_000_000, &timing));
        }
        assert_eq!(steps[0], Step::Send(vec![Out::ParamRequestList]));
        assert_eq!(steps[5], Step::Failed(Failure::Timeout));
    }

    #[test]
    fn a_mission_download_and_upload_follow_the_protocol() {
        let timing = Timing::default();
        let item = |seq| MissionItemInt {
            seq,
            frame: 3,
            command: 16,
            current: false,
            autocontinue: true,
            params: [0.0; 4],
            x: 1,
            y: 2,
            z: 30.0,
            mission_type: 0,
        };
        let (mut down, out) = MissionDownload::start(0);
        assert_eq!(out, [Out::MissionRequestList]);
        assert_eq!(down.count(2, 10), Step::Send(vec![Out::MissionRequest(0)]));
        assert_eq!(
            down.item(item(0), 20),
            Step::Send(vec![Out::MissionRequest(1)])
        );
        // A timeout sends the last request again.
        assert_eq!(
            down.tick(2_000_000, &timing),
            Step::Send(vec![Out::MissionRequest(1)])
        );
        assert_eq!(
            down.item(item(1), 2_000_010),
            Step::Done(vec![item(0), item(1)])
        );

        let (mut up, out) = MissionUpload::start("o1".into(), vec![item(9), item(9)], 0);
        assert_eq!(out, [Out::MissionCount(2)]);
        assert_eq!(
            up.request(1, 10),
            Step::Send(vec![Out::MissionItem(item(1))])
        );
        assert_eq!(up.ack(0), Step::Done(()));
        assert_eq!(
            up.ack(4),
            Step::Failed(Failure::Rejected("No space for that many items".into()))
        );
        for s in 1..=3u64 {
            let _ = up.tick(s * 2_000_000, &timing);
        }
        assert_eq!(up.tick(8_000_000, &timing), Step::Failed(Failure::Timeout));
    }

    #[test]
    fn px4_integers_travel_as_their_bits() {
        // INT32 -1, as PX4 sends CAL_ACC0_PRIO: a NaN as a float.
        let minus_one = f32::from_bits(u32::MAX);
        assert!(minus_one.is_nan());
        assert!((from_bytewise(minus_one, 6) + 1.0).abs() < f32::EPSILON);
        assert!((from_bytewise(f32::from_bits(10040), 6) - 10040.0).abs() < f32::EPSILON);
        assert!((from_bytewise(f32::from_bits(0xff), 2) + 1.0).abs() < f32::EPSILON);
        assert!((from_bytewise(1.5, 9) - 1.5).abs() < f32::EPSILON);
        for (value, kind) in [(-1.0, 6), (10040.0, 6), (200.0, 1), (-3.0, 4), (2.5, 9)] {
            let sent = to_bytewise(value, kind);
            assert!((from_bytewise(sent, kind) - value).abs() < f32::EPSILON);
        }
        assert_eq!(to_bytewise(-1.0, 6).to_bits(), u32::MAX);
    }

    #[test]
    fn parameters_go_up_packed_without_padding_or_defaults() {
        let params: Vec<Param> = [
            ("ATC_RAT_RLL_P", 0.15, 9),
            ("ATC_RAT_PIT_P", 0.15, 9),
            ("FRAME_CLASS", 1.0, 2),
            ("BATT_CAPACITY", 5200.0, 6),
        ]
        .iter()
        .map(|(name, value, kind)| Param {
            name: (*name).into(),
            value: *value,
            kind: *kind,
        })
        .collect();
        let file = encode_param_upload(&params);
        assert_eq!(
            u16::from_le_bytes([file[0], file[1]]),
            0x671b,
            "no defaults"
        );
        assert_eq!(u16::from_le_bytes([file[2], file[3]]), 4);
        assert_eq!(
            usize::from(u16::from_le_bytes([file[4], file[5]])),
            file.len(),
            "the total is the length"
        );
        // It reads back as the parameters, sorted, with no defaults.
        let back = parse_param_pck(&file).unwrap();
        let names: Vec<&str> = back.iter().map(|(p, _)| p.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "ATC_RAT_PIT_P",
                "ATC_RAT_RLL_P",
                "BATT_CAPACITY",
                "FRAME_CLASS"
            ]
        );
        assert!(back.iter().all(|(_, d)| d.is_none()));
        assert!((back[2].0.value - 5200.0).abs() < f32::EPSILON);
        // Entries follow each other: no padding between them.
        let first = 6 + 2 + "ATC_RAT_PIT_P".len() + 4;
        assert_eq!(file[6] & 0x0f, 4, "a float");
        assert_eq!(file[first] & 0x0f, 4, "the next entry starts at once");
        assert_eq!(file[first + 1] & 0x0f, 8, "sharing ATC_RAT_");
    }
}

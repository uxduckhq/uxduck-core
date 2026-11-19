//! The extended parameter protocol with a camera (ADR-0063 Y5, S411): the
//! settings a camera's definition file names are read and written as
//! `PARAM_EXT` messages, addressed to the camera's own component.
//!
//! Reading all of them is `PARAM_EXT_REQUEST_LIST`, answered by one
//! `PARAM_EXT_VALUE` each, with the gaps asked for by index. Reading one is
//! `PARAM_EXT_REQUEST_READ` by name. Writing one is `PARAM_EXT_SET`, which
//! the camera answers with `PARAM_EXT_ACK`: accepted, refused, or still
//! being worked on. Each ask is sent again when nothing answers it, a few
//! times, and then said to have gone unanswered. Nothing here does I/O:
//! `Session` addresses and frames what this asks to send.

use std::collections::{BTreeMap, BTreeSet};

use uxduck_core::domain::camera::{CameraSettingKind, grouped};

use crate::camera::Key;

/// `PARAM_ACK_ACCEPTED`.
pub const ACCEPTED: u8 = 0;
/// `PARAM_ACK_VALUE_UNSUPPORTED`: no such setting, or not that value.
pub const VALUE_UNSUPPORTED: u8 = 1;
/// `PARAM_ACK_FAILED`.
pub const FAILED: u8 = 2;
/// `PARAM_ACK_IN_PROGRESS`: another acknowledgement follows.
pub const IN_PROGRESS: u8 = 3;

/// The most names kept for one camera: a camera that sends more is not
/// believed past this. The names come on its MAVLink stream, and it may
/// say it has 65,535 of them. A list that names more ends by saying how
/// many of its settings were read and how many were not ([`not_read`]),
/// and every read after it says so until a list fits. A setting UXDuck
/// asks for or writes by name is one it chose, from the camera's
/// definition file, so the camera's answer about it is kept all the same.
const NAMES_KEPT: usize = 1024;
/// How often an ask is sent before it counts as unanswered, and how long
/// each waits.
const ATTEMPTS: u8 = 3;
const RETRY_US: u64 = 1_500_000;
/// Silence during a list before its gaps are asked for, and how many at
/// once, so that a slow link is not filled with asks. Every gap is asked
/// for in its turn: an asking that brings a value is followed by the next.
const QUIET_US: u64 = 1_000_000;
const GAPS_AT_ONCE: usize = 16;
/// How many askings in a row may bring nothing before the camera counts as
/// having stopped answering, as [`ATTEMPTS`] is for one ask.
const ROUNDS: u8 = 3;
/// How long a camera that said it is working on a write is waited for.
const WORKING_US: u64 = 30_000_000;

/// A setting's value as it goes on the wire, with its type
/// (`MAV_PARAM_EXT_TYPE`).
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Uint8(u8),
    Int8(i8),
    Uint16(u16),
    Int16(i16),
    Uint32(u32),
    Int32(i32),
    Uint64(u64),
    Int64(i64),
    Real32(f32),
    Real64(f64),
    /// Bytes only the camera's maker can read, without the zeros after
    /// them.
    Custom(Vec<u8>),
}

impl Value {
    /// Its `MAV_PARAM_EXT_TYPE`.
    #[must_use]
    pub fn kind(&self) -> u8 {
        match self {
            Self::Uint8(_) => 1,
            Self::Int8(_) => 2,
            Self::Uint16(_) => 3,
            Self::Int16(_) => 4,
            Self::Uint32(_) => 5,
            Self::Int32(_) => 6,
            Self::Uint64(_) => 7,
            Self::Int64(_) => 8,
            Self::Real32(_) => 9,
            Self::Real64(_) => 10,
            Self::Custom(_) => 11,
        }
    }

    /// The value in a `param_value` field of this `MAV_PARAM_EXT_TYPE`:
    /// the type's own bytes, least significant first. None for a type
    /// that is not one of the protocol's.
    #[must_use]
    pub fn read(kind: u8, bytes: &[u8; 128]) -> Option<Self> {
        let take = |n: usize| -> [u8; 8] {
            let mut out = [0u8; 8];
            out[..n].copy_from_slice(&bytes[..n]);
            out
        };
        Some(match kind {
            1 => Self::Uint8(bytes[0]),
            2 => Self::Int8(i8::from_le_bytes([bytes[0]])),
            3 => Self::Uint16(u16::from_le_bytes([bytes[0], bytes[1]])),
            4 => Self::Int16(i16::from_le_bytes([bytes[0], bytes[1]])),
            5 => Self::Uint32(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
            6 => Self::Int32(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
            7 => Self::Uint64(u64::from_le_bytes(take(8))),
            8 => Self::Int64(i64::from_le_bytes(take(8))),
            9 => Self::Real32(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
            10 => Self::Real64(f64::from_le_bytes(take(8))),
            11 => {
                let end = bytes
                    .iter()
                    .rposition(|byte| *byte != 0)
                    .map_or(0, |at| at + 1);
                Self::Custom(bytes[..end].to_vec())
            }
            _ => return None,
        })
    }

    /// The value as a `param_value` field.
    #[must_use]
    pub fn bytes(&self) -> [u8; 128] {
        let mut out = [0u8; 128];
        let mut put = |bytes: &[u8]| {
            out[..bytes.len().min(128)].copy_from_slice(&bytes[..bytes.len().min(128)]);
        };
        match self {
            Self::Uint8(v) => put(&v.to_le_bytes()),
            Self::Int8(v) => put(&v.to_le_bytes()),
            Self::Uint16(v) => put(&v.to_le_bytes()),
            Self::Int16(v) => put(&v.to_le_bytes()),
            Self::Uint32(v) => put(&v.to_le_bytes()),
            Self::Int32(v) => put(&v.to_le_bytes()),
            Self::Uint64(v) => put(&v.to_le_bytes()),
            Self::Int64(v) => put(&v.to_le_bytes()),
            Self::Real32(v) => put(&v.to_le_bytes()),
            Self::Real64(v) => put(&v.to_le_bytes()),
            Self::Custom(bytes) => put(bytes),
        }
        out
    }

    /// The value as a number. None for a custom one. A 64-bit whole number
    /// past 2^53 is the nearest number to it, and a 32-bit real is the
    /// shortest number that reads back as it: 0.002, not
    /// 0.0020000000949949026.
    #[must_use]
    #[allow(clippy::cast_precision_loss, reason = "said above")]
    pub fn number(&self) -> Option<f64> {
        Some(match self {
            Self::Uint8(v) => f64::from(*v),
            Self::Int8(v) => f64::from(*v),
            Self::Uint16(v) => f64::from(*v),
            Self::Int16(v) => f64::from(*v),
            Self::Uint32(v) => f64::from(*v),
            Self::Int32(v) => f64::from(*v),
            Self::Uint64(v) => *v as f64,
            Self::Int64(v) => *v as f64,
            Self::Real32(v) => v.to_string().parse().unwrap_or_else(|_| f64::from(*v)),
            Self::Real64(v) => *v,
            Self::Custom(_) => return None,
        })
    }

    /// A number as a value of a setting's type. None when the type does
    /// not hold it, as `CameraSettingKind::refuses` says why.
    #[must_use]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "checked against the type's bounds first"
    )]
    pub fn of(kind: CameraSettingKind, value: f64) -> Option<Self> {
        if kind.refuses(value).is_some() {
            return None;
        }
        Some(match kind {
            CameraSettingKind::Bool | CameraSettingKind::Uint8 => Self::Uint8(value as u8),
            CameraSettingKind::Int8 => Self::Int8(value as i8),
            CameraSettingKind::Uint16 => Self::Uint16(value as u16),
            CameraSettingKind::Int16 => Self::Int16(value as i16),
            CameraSettingKind::Uint32 => Self::Uint32(value as u32),
            CameraSettingKind::Int32 => Self::Int32(value as i32),
            CameraSettingKind::Uint64 => Self::Uint64(value as u64),
            CameraSettingKind::Int64 => Self::Int64(value as i64),
            CameraSettingKind::Float => Self::Real32(value as f32),
            CameraSettingKind::Double => Self::Real64(value),
            CameraSettingKind::Custom => return None,
        })
    }
}

/// A `PARAM_EXT_VALUE` as it arrived.
#[derive(Clone, Debug, PartialEq)]
pub struct Said {
    pub name: String,
    pub value: Value,
    pub count: u16,
    pub index: u16,
}

/// A `PARAM_EXT_ACK` as it arrived.
#[derive(Clone, Debug, PartialEq)]
pub struct Acked {
    pub name: String,
    pub value: Value,
    /// `PARAM_ACK`.
    pub result: u8,
}

/// What to send a camera.
#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    /// `PARAM_EXT_REQUEST_LIST`.
    List,
    /// `PARAM_EXT_REQUEST_READ` by name.
    Read(String),
    /// `PARAM_EXT_REQUEST_READ` by its place in the list.
    ReadIndex(u16),
    /// `PARAM_EXT_SET`.
    Set { name: String, value: Value },
}

/// How a write ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Accepted,
    /// The camera said no, in words.
    Refused(String),
    /// No acknowledgement after every attempt.
    Timeout,
}

/// What came of the asks.
#[derive(Clone, Debug, PartialEq)]
pub enum Told {
    /// The values a camera gave since this was last told. `read` ends a
    /// read: every value asked for is in, or why not.
    Values {
        key: Key,
        values: Vec<(String, Value)>,
        read: Option<Result<(), String>>,
    },
    /// A write ended.
    Done {
        receipt_id: String,
        outcome: Outcome,
        attempts: u8,
    },
    /// The camera is working on a write and will answer again.
    Working { receipt_id: String },
}

#[derive(Clone, Debug)]
struct Listing {
    attempts: u8,
    asked_us: u64,
    heard_us: Option<u64>,
    count: Option<u16>,
    got: BTreeSet<u16>,
    /// How many of the settings it gave were not kept, their names being
    /// past [`NAMES_KEPT`].
    unkept: u16,
    /// How many askings for its gaps in a row have brought nothing.
    rounds: u8,
    /// How many values it had given when its gaps were last asked for.
    had: usize,
}

#[derive(Clone, Copy, Debug)]
struct Ask {
    attempts: u8,
    asked_us: u64,
}

#[derive(Clone, Debug)]
struct Write {
    receipt_id: String,
    value: Value,
    attempts: u8,
    sent_us: u64,
    working: bool,
}

#[derive(Clone, Debug, Default)]
struct One {
    list: Option<Listing>,
    reads: BTreeMap<String, Ask>,
    writes: BTreeMap<String, Write>,
    /// Every name it gave, to stop at [`NAMES_KEPT`].
    names: BTreeSet<String>,
    /// The values it gave since they were last told.
    heard: BTreeMap<String, Value>,
    /// A read that ended since.
    ended: Option<Result<(), String>>,
    /// What its last list left unread, its names being more than are
    /// kept. A read that ends well still says so.
    cut: Option<String>,
}

impl One {
    /// A read that nothing is left of has ended, unless it ended already:
    /// well, or with what the last list left unread.
    fn settle(&mut self) {
        if self.list.is_none() && self.reads.is_empty() && self.ended.is_none() {
            self.ended = Some(self.cut.clone().map_or(Ok(()), Err));
        }
    }
}

/// How many settings a camera's list said it has, in words.
fn listed(count: u16) -> String {
    format!("The camera listed {} settings.", grouped(u64::from(count)))
}

/// What a list left unread because its names are more than are kept, in
/// words. `first` says how many settings the camera gave, and `given` is
/// that number.
fn not_read(first: &str, given: usize, unkept: u16) -> Option<String> {
    let read = given.saturating_sub(usize::from(unkept));
    (unkept > 0).then(|| {
        format!(
            "{first} UXDuck read {} of them and not the other {}, because it keeps at most {} names from one camera, whose list is its own to send. A setting here that shows no value may be one it did not read.",
            grouped(read as u64),
            grouped(u64::from(unkept)),
            grouped(NAMES_KEPT as u64),
        )
    })
}

/// The cameras whose settings are read and written, by the component
/// that answers for them.
#[derive(Clone, Debug, Default)]
pub struct Params {
    cameras: BTreeMap<Key, One>,
}

impl Params {
    /// Asks a camera for every setting it has.
    pub fn list(&mut self, key: &Key, now_us: u64) -> Vec<Out> {
        let one = self.cameras.entry(key.clone()).or_default();
        one.ended = None;
        one.list = Some(Listing {
            attempts: 1,
            asked_us: now_us,
            heard_us: None,
            count: None,
            got: BTreeSet::new(),
            unkept: 0,
            rounds: 0,
            had: 0,
        });
        vec![Out::List]
    }

    /// Asks a camera for the settings named.
    pub fn read(&mut self, key: &Key, names: Vec<String>, now_us: u64) -> Vec<Out> {
        let one = self.cameras.entry(key.clone()).or_default();
        let mut out = Vec::new();
        for name in names {
            if name.is_empty() || name.len() > 16 || !name.is_ascii() {
                continue;
            }
            one.ended = None;
            one.reads.insert(
                name.clone(),
                Ask {
                    attempts: 1,
                    asked_us: now_us,
                },
            );
            out.push(Out::Read(name));
        }
        // A read that names nothing to ask for has ended.
        if out.is_empty() {
            one.settle();
        }
        out
    }

    /// Writes one setting. A write of the same setting still waiting for
    /// its answer is in the way.
    ///
    /// # Errors
    /// Why it is not sent.
    pub fn set(
        &mut self,
        key: &Key,
        receipt_id: String,
        name: &str,
        value: Value,
        now_us: u64,
    ) -> Result<Vec<Out>, &'static str> {
        let one = self.cameras.entry(key.clone()).or_default();
        if one.writes.contains_key(name) {
            return Err("the same setting is still waiting for the camera's answer");
        }
        one.writes.insert(
            name.to_owned(),
            Write {
                receipt_id,
                value: value.clone(),
                attempts: 1,
                sent_us: now_us,
                working: false,
            },
        );
        Ok(vec![Out::Set {
            name: name.to_owned(),
            value,
        }])
    }

    /// A camera gave a value: one of a list, an answer to a read, or one
    /// it sends unasked. Only a camera that was asked something is heard.
    pub fn value(&mut self, key: &Key, said: Said, now_us: u64) {
        let Some(one) = self.cameras.get_mut(key) else {
            return;
        };
        // A value asked for by name is kept whatever else the camera named.
        let kept = one.reads.contains_key(&said.name)
            || one.names.contains(&said.name)
            || (one.names.len() < NAMES_KEPT && one.names.insert(said.name.clone()));
        let asked = one.list.is_some() || !one.reads.is_empty();
        // A value whose name is not kept still counts as given, so that
        // the list ends when the camera has given it all and is not asked
        // again for what it sent.
        if let Some(list) = &mut one.list {
            list.heard_us = Some(now_us);
            list.count = Some(said.count);
            if said.index < said.count && list.got.insert(said.index) && !kept {
                list.unkept += 1;
            }
            if list.got.len() >= usize::from(said.count) {
                one.cut = not_read(&listed(said.count), list.got.len(), list.unkept);
                one.list = None;
            }
        }
        if kept {
            one.reads.remove(&said.name);
            one.heard.insert(said.name, said.value);
        }
        // A value sent unasked ends no read.
        if asked {
            one.settle();
        }
    }

    /// A camera acknowledged a write: its own answer to one of these
    /// writes, or what it says to anyone who wrote a setting.
    pub fn ack(&mut self, key: &Key, acked: Acked, now_us: u64) -> Vec<Told> {
        let Some(one) = self.cameras.get_mut(key) else {
            return Vec::new();
        };
        let mut told = Vec::new();
        if acked.result == IN_PROGRESS {
            if let Some(write) = one.writes.get_mut(&acked.name) {
                let first = !write.working;
                write.working = true;
                write.sent_us = now_us;
                if first {
                    told.push(Told::Working {
                        receipt_id: write.receipt_id.clone(),
                    });
                }
            }
            return told;
        }
        let written = one.writes.remove(&acked.name);
        let ours = written.is_some();
        if let Some(write) = written {
            let outcome = match acked.result {
                ACCEPTED => Outcome::Accepted,
                VALUE_UNSUPPORTED => Outcome::Refused(
                    "It does not have this setting, or does not take this value for it. The camera said: Value unsupported.".into(),
                ),
                FAILED => Outcome::Refused("The camera said: Failed.".into()),
                other => Outcome::Refused(format!("The camera said: result {other}.")),
            };
            told.push(Told::Done {
                receipt_id: write.receipt_id,
                outcome,
                attempts: write.attempts,
            });
        }
        // An accepted write carries the value the camera now has. A
        // refusal carries the one it keeps, when it knows the setting. The
        // answer to a write of UXDuck's own is kept whatever else the
        // camera named.
        if (acked.result == ACCEPTED || acked.result == FAILED)
            && (ours
                || one.names.contains(&acked.name)
                || (one.names.len() < NAMES_KEPT && one.names.insert(acked.name.clone())))
        {
            one.heard.insert(acked.name, acked.value);
        }
        told
    }

    /// Each tick: what to send again, and what went unanswered.
    pub fn tick(&mut self, now_us: u64) -> (Vec<(Key, Out)>, Vec<Told>) {
        let mut outs = Vec::new();
        let mut told = Vec::new();
        for (key, one) in &mut self.cameras {
            let waited = |since: u64, wait: u64| now_us.saturating_sub(since) >= wait;
            if let Some(list) = &mut one.list {
                match list.heard_us {
                    // Nothing yet: the request is sent again.
                    None if waited(list.asked_us, RETRY_US) => {
                        if list.attempts < ATTEMPTS {
                            list.attempts += 1;
                            list.asked_us = now_us;
                            outs.push((key.clone(), Out::List));
                        } else {
                            one.list = None;
                            one.ended = Some(Err("The camera did not answer.".into()));
                        }
                    }
                    // It went quiet with some missing: those are asked for.
                    Some(heard) if waited(heard.max(list.asked_us), QUIET_US) => {
                        let count = list.count.unwrap_or(0);
                        let missing: Vec<u16> = (0..count)
                            .filter(|index| !list.got.contains(index))
                            .collect();
                        // An asking that brought a value was answered, so
                        // the camera is asked on. Its count is 16 bits,
                        // which is as long as that goes on.
                        if list.got.len() > list.had {
                            list.rounds = 0;
                        }
                        if list.rounds < ROUNDS && !missing.is_empty() {
                            list.rounds += 1;
                            list.had = list.got.len();
                            list.asked_us = now_us;
                            for index in missing.into_iter().take(GAPS_AT_ONCE) {
                                outs.push((key.clone(), Out::ReadIndex(index)));
                            }
                        } else {
                            let (got, unkept) = (list.got.len(), list.unkept);
                            one.list = None;
                            one.ended = Some(if missing.is_empty() {
                                one.cut = not_read(&listed(count), got, unkept);
                                one.cut.clone().map_or(Ok(()), Err)
                            } else {
                                let stopped = format!(
                                    "The camera gave {} of its {} settings and stopped answering.",
                                    grouped(got as u64),
                                    grouped(u64::from(count))
                                );
                                one.cut = not_read(&stopped, got, unkept);
                                Err(one.cut.clone().unwrap_or(stopped))
                            });
                        }
                    }
                    _ => {}
                }
            }
            let mut unanswered = false;
            one.reads.retain(|name, ask| {
                if !waited(ask.asked_us, RETRY_US) {
                    return true;
                }
                if ask.attempts >= ATTEMPTS {
                    unanswered = true;
                    return false;
                }
                ask.attempts += 1;
                ask.asked_us = now_us;
                outs.push((key.clone(), Out::Read(name.clone())));
                true
            });
            if unanswered {
                one.ended = Some(Err("The camera did not answer.".into()));
            }
            one.writes.retain(|name, write| {
                let wait = if write.working { WORKING_US } else { RETRY_US };
                if !waited(write.sent_us, wait) {
                    return true;
                }
                if write.working || write.attempts >= ATTEMPTS {
                    told.push(Told::Done {
                        receipt_id: write.receipt_id.clone(),
                        outcome: Outcome::Timeout,
                        attempts: write.attempts,
                    });
                    return false;
                }
                write.attempts += 1;
                write.sent_us = now_us;
                outs.push((
                    key.clone(),
                    Out::Set {
                        name: name.clone(),
                        value: write.value.clone(),
                    },
                ));
                true
            });
        }
        (outs, told)
    }

    /// The values given and the reads ended since this was last asked.
    pub fn take(&mut self) -> Vec<Told> {
        let mut told = Vec::new();
        for (key, one) in &mut self.cameras {
            if one.heard.is_empty() && one.ended.is_none() {
                continue;
            }
            told.push(Told::Values {
                key: key.clone(),
                values: std::mem::take(&mut one.heard).into_iter().collect(),
                read: one.ended.take(),
            });
        }
        told
    }

    /// Whether anything is still waited for, so that time must go on.
    #[must_use]
    pub fn waiting(&self) -> bool {
        self.cameras
            .values()
            .any(|one| one.list.is_some() || !one.reads.is_empty() || !one.writes.is_empty())
    }

    /// Forgets the cameras at the addresses that match. A write still
    /// waiting there ends unanswered.
    pub fn forget(&mut self, gone: impl Fn(&Key) -> bool) -> Vec<Told> {
        let mut told = Vec::new();
        self.cameras.retain(|key, one| {
            if !gone(key) {
                return true;
            }
            for write in one.writes.values() {
                told.push(Told::Done {
                    receipt_id: write.receipt_id.clone(),
                    outcome: Outcome::Timeout,
                    attempts: write.attempts,
                });
            }
            false
        });
        told
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> Key {
        ("link".into(), "peer".into(), 1, 100)
    }

    fn said(name: &str, value: Value, index: u16, count: u16) -> Said {
        Said {
            name: name.into(),
            value,
            count,
            index,
        }
    }

    #[test]
    fn every_type_goes_on_the_wire_and_comes_back_the_same() {
        let values = [
            Value::Uint8(200),
            Value::Int8(-5),
            Value::Uint16(40_000),
            Value::Int16(-1234),
            Value::Uint32(3_000_000_000),
            Value::Int32(-2_000_000_000),
            Value::Uint64(u64::MAX - 1),
            Value::Int64(i64::MIN + 1),
            Value::Real32(0.016_666),
            Value::Real64(-2.5e-9),
            Value::Custom(b"abc".to_vec()),
        ];
        for value in values {
            assert_eq!(Value::read(value.kind(), &value.bytes()), Some(value));
        }
        assert_eq!(Value::read(0, &[0; 128]), None);
        assert_eq!(Value::read(12, &[0; 128]), None);
    }

    #[test]
    fn a_number_becomes_the_settings_type_or_is_refused() {
        use CameraSettingKind as Kind;
        assert_eq!(Value::of(Kind::Bool, 1.0), Some(Value::Uint8(1)));
        assert_eq!(Value::of(Kind::Bool, 2.0), None);
        assert_eq!(Value::of(Kind::Uint32, 3200.0), Some(Value::Uint32(3200)));
        assert_eq!(Value::of(Kind::Uint32, -1.0), None);
        assert_eq!(Value::of(Kind::Uint8, 1.5), None);
        assert_eq!(Value::of(Kind::Int16, -40_000.0), None);
        assert_eq!(Value::of(Kind::Float, 0.5), Some(Value::Real32(0.5)));
        assert_eq!(Value::of(Kind::Double, f64::NAN), None);
        assert_eq!(Value::of(Kind::Custom, 1.0), None);
    }

    #[test]
    fn a_list_ends_when_every_value_is_in() {
        let mut params = Params::default();
        assert_eq!(params.list(&key(), 0), vec![Out::List]);
        params.value(&key(), said("CAM_MODE", Value::Uint32(1), 0, 2), 10);
        assert_eq!(
            params.take(),
            vec![Told::Values {
                key: key(),
                values: vec![("CAM_MODE".into(), Value::Uint32(1))],
                read: None,
            }]
        );
        params.value(&key(), said("CAM_ISO", Value::Uint32(100), 1, 2), 20);
        assert_eq!(
            params.take(),
            vec![Told::Values {
                key: key(),
                values: vec![("CAM_ISO".into(), Value::Uint32(100))],
                read: Some(Ok(())),
            }]
        );
        assert!(!params.waiting());
        assert!(params.take().is_empty());
    }

    #[test]
    fn a_list_nobody_answers_is_asked_three_times_and_then_said_unanswered() {
        let mut params = Params::default();
        params.list(&key(), 0);
        assert_eq!(params.tick(RETRY_US - 1).0, vec![]);
        assert_eq!(params.tick(RETRY_US).0, vec![(key(), Out::List)]);
        assert_eq!(params.tick(2 * RETRY_US).0, vec![(key(), Out::List)]);
        assert_eq!(params.tick(3 * RETRY_US).0, vec![]);
        assert_eq!(
            params.take(),
            vec![Told::Values {
                key: key(),
                values: vec![],
                read: Some(Err("The camera did not answer.".into())),
            }]
        );
    }

    #[test]
    fn a_lists_gaps_are_asked_for_by_index() {
        let mut params = Params::default();
        params.list(&key(), 0);
        params.value(&key(), said("A", Value::Uint8(1), 0, 3), 10);
        params.value(&key(), said("C", Value::Uint8(3), 2, 3), 20);
        assert_eq!(params.tick(20 + QUIET_US - 1).0, vec![]);
        assert_eq!(
            params.tick(20 + QUIET_US).0,
            vec![(key(), Out::ReadIndex(1))]
        );
        params.value(&key(), said("B", Value::Uint8(2), 1, 3), 30 + QUIET_US);
        let told = params.take();
        assert!(
            matches!(&told[..], [Told::Values { values, read: Some(Ok(())), .. }] if values.len() == 3),
            "{told:?}"
        );
    }

    // A list with more gaps than are asked for at once, or than three
    // askings fill, is read whole: a camera that answers each asking is
    // asked on, and is not said to have stopped answering.
    #[test]
    fn a_camera_that_answers_every_asking_is_asked_for_every_gap() {
        let mut params = Params::default();
        params.list(&key(), 0);
        let count = 200_u16;
        let give = |params: &mut Params, index: u16, now: u64| {
            params.value(
                &key(),
                said(&format!("S{index}"), Value::Uint8(1), index, count),
                now,
            );
        };
        // It gave one in two, as over a radio that drops the rest.
        for index in (0..count).step_by(2) {
            give(&mut params, index, 0);
        }
        let mut now = 0;
        let mut askings = 0;
        loop {
            now += QUIET_US;
            let asked: Vec<u16> = params
                .tick(now)
                .0
                .into_iter()
                .filter_map(|(_, out)| match out {
                    Out::ReadIndex(index) => Some(index),
                    _ => None,
                })
                .collect();
            if asked.is_empty() {
                break;
            }
            assert!(asked.len() <= GAPS_AT_ONCE);
            askings += 1;
            assert!(askings <= 100, "asked without end");
            for index in asked {
                give(&mut params, index, now);
            }
        }
        // 100 gaps, 16 at an asking.
        assert_eq!(askings, 7);
        let told = params.take();
        assert!(
            matches!(&told[..], [Told::Values { values, read: Some(Ok(())), .. }] if values.len() == 200),
            "{told:?}"
        );
    }

    // One that answers some askings and then none is asked three times
    // for what is left, and only then said to have stopped.
    #[test]
    fn a_camera_that_stops_part_way_through_its_gaps_is_asked_three_times_more() {
        let mut params = Params::default();
        params.list(&key(), 0);
        let count = 100_u16;
        for index in 0..40 {
            params.value(
                &key(),
                said(&format!("S{index}"), Value::Uint8(1), index, count),
                0,
            );
        }
        let mut now = 0;
        let mut askings = 0;
        loop {
            now += QUIET_US;
            let asked = params.tick(now).0;
            if asked.is_empty() {
                break;
            }
            askings += 1;
            assert!(askings <= 100, "asked without end");
            // It answers the first two askings and no more.
            if askings <= 2 {
                for (_, out) in asked {
                    if let Out::ReadIndex(index) = out {
                        params.value(
                            &key(),
                            said(&format!("S{index}"), Value::Uint8(1), index, count),
                            now,
                        );
                    }
                }
            }
        }
        assert_eq!(askings, 2 + usize::from(ROUNDS));
        let told = params.take();
        assert!(
            matches!(&told[..], [Told::Values { read: Some(Err(why)), .. }]
                if why == "The camera gave 72 of its 100 settings and stopped answering."),
            "{told:?}"
        );
    }

    #[test]
    fn a_list_that_stops_short_says_how_far_it_got() {
        let mut params = Params::default();
        params.list(&key(), 0);
        params.value(&key(), said("A", Value::Uint8(1), 0, 3), 0);
        let mut now = 0;
        for _ in 0..=ROUNDS {
            now += QUIET_US;
            params.tick(now);
        }
        let told = params.take();
        assert!(
            matches!(&told[..], [Told::Values { read: Some(Err(why)), .. }]
                if why == "The camera gave 1 of its 3 settings and stopped answering."),
            "{told:?}"
        );
    }

    #[test]
    fn a_read_is_asked_again_and_ends_with_its_value() {
        let mut params = Params::default();
        assert_eq!(
            params.read(&key(), vec!["CAM_ISO".into(), String::new()], 0),
            vec![Out::Read("CAM_ISO".into())]
        );
        assert_eq!(
            params.tick(RETRY_US).0,
            vec![(key(), Out::Read("CAM_ISO".into()))]
        );
        // A value read alone has no place in a list.
        params.value(
            &key(),
            said("CAM_ISO", Value::Uint32(400), u16::MAX, 9),
            RETRY_US + 5,
        );
        assert_eq!(
            params.take(),
            vec![Told::Values {
                key: key(),
                values: vec![("CAM_ISO".into(), Value::Uint32(400))],
                read: Some(Ok(())),
            }]
        );
    }

    #[test]
    fn a_write_is_accepted_refused_or_never_answered() {
        let mut params = Params::default();
        let set = |params: &mut Params, receipt: &str, now| {
            params.set(&key(), receipt.into(), "CAM_ISO", Value::Uint32(400), now)
        };
        assert!(set(&mut params, "r1", 0).is_ok());
        assert_eq!(
            set(&mut params, "r2", 1),
            Err("the same setting is still waiting for the camera's answer")
        );
        let ack = |result| Acked {
            name: "CAM_ISO".into(),
            value: Value::Uint32(400),
            result,
        };
        assert_eq!(
            params.ack(&key(), ack(ACCEPTED), 5),
            vec![Told::Done {
                receipt_id: "r1".into(),
                outcome: Outcome::Accepted,
                attempts: 1,
            }]
        );
        // The value the camera now has is told with it.
        assert!(matches!(&params.take()[..], [Told::Values { values, .. }] if values.len() == 1));

        assert!(set(&mut params, "r3", 10).is_ok());
        let refused = params.ack(&key(), ack(FAILED), 11);
        assert!(
            matches!(&refused[..], [Told::Done { outcome: Outcome::Refused(why), .. }]
                if why == "The camera said: Failed."),
            "{refused:?}"
        );

        assert!(set(&mut params, "r4", 20).is_ok());
        let (again, _) = params.tick(20 + RETRY_US);
        assert!(matches!(&again[..], [(_, Out::Set { .. })]));
        params.tick(20 + 2 * RETRY_US);
        let (_, told) = params.tick(20 + 3 * RETRY_US);
        assert_eq!(
            told,
            vec![Told::Done {
                receipt_id: "r4".into(),
                outcome: Outcome::Timeout,
                attempts: 3,
            }]
        );
    }

    #[test]
    fn a_write_the_camera_is_working_on_waits_for_its_end() {
        let mut params = Params::default();
        params
            .set(&key(), "r1".into(), "CAM_ISO", Value::Uint32(400), 0)
            .unwrap();
        let ack = |result| Acked {
            name: "CAM_ISO".into(),
            value: Value::Uint32(400),
            result,
        };
        assert_eq!(
            params.ack(&key(), ack(IN_PROGRESS), 5),
            vec![Told::Working {
                receipt_id: "r1".into()
            }]
        );
        // Not sent again while the camera works on it.
        assert_eq!(params.tick(5 + 10 * RETRY_US), (vec![], vec![]));
        assert!(matches!(
            &params.ack(&key(), ack(ACCEPTED), 6 + 10 * RETRY_US)[..],
            [Told::Done {
                outcome: Outcome::Accepted,
                ..
            }]
        ));
    }

    #[test]
    fn a_camera_that_was_asked_nothing_is_not_heard() {
        let mut params = Params::default();
        params.value(&key(), said("CAM_ISO", Value::Uint32(1), 0, 1), 0);
        assert!(params.take().is_empty());
    }

    #[test]
    fn names_past_the_limit_are_not_kept() {
        let mut params = Params::default();
        params.list(&key(), 0);
        for index in 0..=NAMES_KEPT {
            let name = format!("N{index}");
            params.value(&key(), said(&name, Value::Uint8(0), u16::MAX, u16::MAX), 1);
        }
        let told = params.take();
        assert!(
            matches!(&told[..], [Told::Values { values, .. }] if values.len() == NAMES_KEPT),
            "{}",
            told.len()
        );
    }

    // A camera lists more settings than UXDuck keeps the names of: the
    // read ends when the camera has given them all, and says how many it
    // read and how many it did not.
    #[test]
    fn a_list_longer_than_the_names_kept_ends_saying_what_was_not_read() {
        let mut params = Params::default();
        params.list(&key(), 0);
        let count = u16::try_from(NAMES_KEPT + 10).unwrap();
        for index in 0..count {
            let name = format!("N{index}");
            params.value(&key(), said(&name, Value::Uint8(0), index, count), 1);
        }
        assert!(!params.waiting(), "the camera gave every one it listed");
        let told = params.take();
        let [Told::Values { values, read, .. }] = &told[..] else {
            panic!("{}", told.len());
        };
        assert_eq!(values.len(), NAMES_KEPT);
        assert_eq!(
            read.clone(),
            Some(Err(
                "The camera listed 1,034 settings. UXDuck read 1,024 of them and not the other 10, because it keeps at most 1,024 names from one camera, whose list is its own to send. A setting here that shows no value may be one it did not read."
                    .to_owned()
            ))
        );
        // Nothing is asked for again: no value is missing from the list.
        let (outs, told) = params.tick(1 + 10 * QUIET_US);
        assert!(outs.is_empty() && told.is_empty());

        // A setting UXDuck asks for by name is one it chose: its value is
        // kept past the limit, and the read still says what the list left.
        assert_eq!(
            params.read(&key(), vec!["N1030".into()], 20 * QUIET_US),
            [Out::Read("N1030".into())]
        );
        params.value(
            &key(),
            said("N1030", Value::Uint8(7), 1030, count),
            20 * QUIET_US + 1,
        );
        assert!(!params.waiting());
        let told = params.take();
        let [Told::Values { values, read, .. }] = &told[..] else {
            panic!("{}", told.len());
        };
        assert_eq!(values[..], [("N1030".to_owned(), Value::Uint8(7))]);
        assert!(
            matches!(read, Some(Err(why)) if why.contains("not the other 10")),
            "{read:?}"
        );

        // A new list that fits says nothing of the last one.
        let mut params = Params::default();
        params.list(&key(), 0);
        for index in 0..3 {
            let name = format!("N{index}");
            params.value(&key(), said(&name, Value::Uint8(0), index, 3), 1);
        }
        assert!(matches!(
            &params.take()[..],
            [Told::Values {
                read: Some(Ok(())),
                ..
            }]
        ));
    }

    // A camera that stops part of the way through a list longer than the
    // names kept says both: what it did not give, and what was not read of
    // what it gave.
    #[test]
    fn a_long_list_that_stops_says_what_was_given_and_what_was_read() {
        let mut params = Params::default();
        params.list(&key(), 0);
        let count = u16::try_from(NAMES_KEPT + 10).unwrap();
        for index in 0..count - 4 {
            let name = format!("N{index}");
            params.value(&key(), said(&name, Value::Uint8(0), index, count), 1);
        }
        let mut now = 1;
        let mut asked = Vec::new();
        for _ in 0..=ROUNDS {
            now += QUIET_US;
            asked.extend(params.tick(now).0.into_iter().map(|(_, out)| out));
        }
        assert_eq!(
            asked.len(),
            usize::from(ROUNDS) * 4,
            "only what it did not give"
        );
        assert!(matches!(asked[0], Out::ReadIndex(1030)));
        let told = params.take();
        let [Told::Values { read, .. }] = &told[..] else {
            panic!("{}", told.len());
        };
        assert_eq!(
            read.clone(),
            Some(Err(
                "The camera gave 1,030 of its 1,034 settings and stopped answering. UXDuck read 1,024 of them and not the other 6, because it keeps at most 1,024 names from one camera, whose list is its own to send. A setting here that shows no value may be one it did not read."
                    .to_owned()
            ))
        );
    }

    // The camera's answer to a write of UXDuck's own is kept, though the
    // setting's name came after the last name kept.
    #[test]
    fn a_write_to_a_setting_past_the_names_kept_keeps_its_answer() {
        let mut params = Params::default();
        params.list(&key(), 0);
        let count = u16::try_from(NAMES_KEPT + 1).unwrap();
        for index in 0..count {
            let name = format!("N{index}");
            params.value(&key(), said(&name, Value::Uint8(0), index, count), 1);
        }
        params.take();
        params
            .set(&key(), "r1".into(), "N1024", Value::Uint8(9), 2)
            .unwrap();
        let acked = Acked {
            name: "N1024".into(),
            value: Value::Uint8(9),
            result: ACCEPTED,
        };
        assert!(matches!(
            &params.ack(&key(), acked.clone(), 3)[..],
            [Told::Done {
                outcome: Outcome::Accepted,
                ..
            }]
        ));
        let told = params.take();
        assert!(
            matches!(&told[..], [Told::Values { values, .. }] if values[..] == [("N1024".to_owned(), Value::Uint8(9))]),
            "{told:?}"
        );
        // Another station's write of a name not kept stays unkept.
        let other = Acked {
            name: "N9999".into(),
            ..acked
        };
        assert!(params.ack(&key(), other, 4).is_empty());
        assert!(params.take().is_empty());
    }

    #[test]
    fn a_forgotten_cameras_write_ends_unanswered() {
        let mut params = Params::default();
        params
            .set(&key(), "r1".into(), "CAM_ISO", Value::Uint32(400), 0)
            .unwrap();
        assert_eq!(
            params.forget(|_| true),
            vec![Told::Done {
                receipt_id: "r1".into(),
                outcome: Outcome::Timeout,
                attempts: 1,
            }]
        );
    }
}

//! ArduPilot's MAVLink dialect as a small table, to decode any of its
//! messages by MAVLink's own names where the whole definitions are too large
//! to carry: in the browser's daemon, and in the installed one before it
//! turns to [`crate::named`] for plugins' messages. Each message has its id,
//! its name, the dialect whose page on mavlink.io documents it, and its
//! fields in declared order; no descriptions, units or enums.
//!
//! The table, `dialect/ardupilotmega.rs`, is written by
//! `cargo xtask mavlink-dialect` from mavio's definitions, and a test with
//! the `named` feature fails once the two differ.

use serde_json::{Map, Number, Value};

mod ardupilotmega;

/// Where mavlink.io documents a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Common,
    Ardupilotmega,
}

impl Dialect {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Common => "common",
            Self::Ardupilotmega => "ardupilotmega",
        }
    }
}

/// One message: its id, its name, its dialect, and its fields as
/// `name:type`, in declared order and separated by spaces, those after `|`
/// being extensions. Types are `u8` to `u64`, `i8` to `i64`, `f32`, `f64`
/// and `char`, or arrays of them such as `char[16]`.
#[derive(Debug)]
pub struct Message(u32, &'static str, Dialect, &'static str);

impl Message {
    #[must_use]
    pub const fn id(&self) -> u32 {
        self.0
    }

    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.1
    }

    #[must_use]
    pub const fn dialect(&self) -> Dialect {
        self.2
    }

    /// The fields in declared order.
    pub fn fields(&self) -> impl Iterator<Item = Field> {
        let mut extension = false;
        self.3.split(' ').filter_map(move |token| {
            if token == "|" {
                extension = true;
                return None;
            }
            let (name, kind) = token.split_once(':')?;
            let (kind, length) = match kind.split_once('[') {
                Some((kind, length)) => (kind, Some(length.strip_suffix(']')?.parse().ok()?)),
                None => (kind, None),
            };
            Some(Field {
                name,
                kind: Kind::parse(kind)?,
                length,
                extension,
            })
        })
    }
}

/// A field's type, or its items' for an array.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    U8,
    I8,
    U16,
    I16,
    U32,
    I32,
    U64,
    I64,
    F32,
    F64,
    Char,
}

impl Kind {
    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "u8" => Self::U8,
            "i8" => Self::I8,
            "u16" => Self::U16,
            "i16" => Self::I16,
            "u32" => Self::U32,
            "i32" => Self::I32,
            "u64" => Self::U64,
            "i64" => Self::I64,
            "f32" => Self::F32,
            "f64" => Self::F64,
            "char" => Self::Char,
            _ => return None,
        })
    }

    #[must_use]
    pub const fn size(self) -> usize {
        match self {
            Self::U8 | Self::I8 | Self::Char => 1,
            Self::U16 | Self::I16 => 2,
            Self::U32 | Self::I32 | Self::F32 => 4,
            Self::U64 | Self::I64 | Self::F64 => 8,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field {
    pub name: &'static str,
    pub kind: Kind,
    /// Items in an array; None for one value.
    pub length: Option<usize>,
    /// Declared after `<extensions/>`: sent last, and only by MAVLink 2.
    pub extension: bool,
}

impl Field {
    /// Bytes in a payload.
    #[must_use]
    pub const fn size(&self) -> usize {
        match self.length {
            Some(length) => self.kind.size() * length,
            None => self.kind.size(),
        }
    }
}

/// Every message, by id.
#[must_use]
pub fn messages() -> &'static [Message] {
    ardupilotmega::MESSAGES
}

#[must_use]
pub fn message(id: u32) -> Option<&'static Message> {
    let at = ardupilotmega::MESSAGES
        .binary_search_by_key(&id, Message::id)
        .ok()?;
    ardupilotmega::MESSAGES.get(at)
}

/// A decoded message: its name, its dialect and its fields by MAVLink's
/// names, with raw values in MAVLink's units.
#[derive(Clone, Debug, PartialEq)]
pub struct Decoded {
    pub name: &'static str,
    pub dialect: Dialect,
    pub fields: Map<String, Value>,
}

/// Decodes a payload, as [`crate::named::decode`] does: MAVLink 2 trims
/// trailing zero bytes and they are put back; text is up to its first zero
/// byte; floats that are not numbers are null. None for an id the dialect
/// does not have.
#[must_use]
pub fn decode(id: u32, payload: &[u8]) -> Option<Decoded> {
    let message = message(id)?;
    // On the wire, the fields before the extensions go largest type first,
    // otherwise in declared order; the extensions follow as declared.
    let mut wire: Vec<Field> = message.fields().collect();
    let base = wire.iter().position(|f| f.extension).unwrap_or(wire.len());
    wire[..base].sort_by_key(|f| std::cmp::Reverse(f.kind.size()));
    let size = wire.iter().map(Field::size).sum::<usize>();
    let mut bytes = payload.to_vec();
    bytes.resize(size.max(payload.len()), 0);
    let mut at = 0;
    let mut fields = Map::new();
    for field in wire {
        let end = at + field.size();
        fields.insert(field.name.to_owned(), read(&field, &bytes[at..end]));
        at = end;
    }
    Some(Decoded {
        name: message.name(),
        dialect: message.dialect(),
        fields,
    })
}

fn read(field: &Field, bytes: &[u8]) -> Value {
    match (field.kind, field.length) {
        (Kind::Char, Some(_)) => {
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            Value::String(String::from_utf8_lossy(&bytes[..end]).into_owned())
        }
        (kind, Some(_)) => Value::Array(
            bytes
                .chunks(kind.size())
                .map(|item| scalar(kind, item))
                .collect(),
        ),
        (kind, None) => scalar(kind, bytes),
    }
}

fn scalar(kind: Kind, bytes: &[u8]) -> Value {
    fn le<const N: usize>(bytes: &[u8]) -> [u8; N] {
        let mut out = [0; N];
        out.copy_from_slice(&bytes[..N]);
        out
    }
    match kind {
        Kind::U8 | Kind::Char => Value::from(bytes[0]),
        Kind::I8 => Value::from(i8::from_le_bytes(le(bytes))),
        Kind::U16 => Value::from(u16::from_le_bytes(le(bytes))),
        Kind::I16 => Value::from(i16::from_le_bytes(le(bytes))),
        Kind::U32 => Value::from(u32::from_le_bytes(le(bytes))),
        Kind::I32 => Value::from(i32::from_le_bytes(le(bytes))),
        Kind::U64 => Value::from(u64::from_le_bytes(le(bytes))),
        Kind::I64 => Value::from(i64::from_le_bytes(le(bytes))),
        // NaN and infinities have no JSON number: null, as MAVLink often
        // means "unknown" by NaN.
        Kind::F32 => Number::from_f64(f64::from(f32::from_le_bytes(le(bytes))))
            .map_or(Value::Null, Value::Number),
        Kind::F64 => {
            Number::from_f64(f64::from_le_bytes(le(bytes))).map_or(Value::Null, Value::Number)
        }
    }
}

/// The table's source, from mavio's definitions of ArduPilot's dialect:
/// what `cargo xtask mavlink-dialect` writes to `dialect/ardupilotmega.rs`.
#[cfg(feature = "named")]
#[must_use]
pub fn generate() -> String {
    use std::fmt::Write as _;

    let protocol = mavio::definitions::protocol();
    let ids = |dialect: &str| -> Vec<&'static mavinspect::protocol::Message> {
        protocol
            .get_dialect_by_name(dialect)
            .map(|d| d.messages().into_iter().collect())
            .unwrap_or_default()
    };
    let common: std::collections::HashSet<u32> = ids("common")
        .into_iter()
        .map(mavinspect::protocol::Message::id)
        .collect();
    let mut messages = ids("ardupilotmega");
    messages.sort_by_key(|m| m.id());
    let mut out = String::from(
        "//! ArduPilot's MAVLink dialect, ardupilotmega and what it includes, by id:\n\
         //! see [`super::Message`]. Written by `cargo xtask mavlink-dialect` from\n\
         //! mavio's definitions; not edited by hand.\n\
         \n\
         use super::Dialect::{Ardupilotmega as A, Common as C};\n\
         use super::Message as M;\n\
         \n\
         #[rustfmt::skip]\n\
         pub(super) static MESSAGES: &[M] = &[\n",
    );
    for message in messages {
        let mut fields = Vec::new();
        let mut extended = false;
        for field in message.fields() {
            if field.extension() && !extended {
                fields.push("|".to_owned());
                extended = true;
            }
            fields.push(format!("{}:{}", field.name(), type_of(field.r#type())));
        }
        let dialect = if common.contains(&message.id()) {
            "C"
        } else {
            "A"
        };
        let _ = writeln!(
            out,
            "    M({}, \"{}\", {dialect}, \"{}\"),",
            message.id(),
            message.name(),
            fields.join(" ")
        );
    }
    out.push_str("];\n");
    out
}

/// A field's type as the table writes it. The version byte of HEARTBEAT is
/// read as any other byte.
#[cfg(feature = "named")]
fn type_of(kind: &mavinspect::protocol::MavType) -> String {
    use mavinspect::protocol::MavType;
    match kind {
        MavType::UInt8 | MavType::UInt8MavlinkVersion => "u8".into(),
        MavType::UInt16 => "u16".into(),
        MavType::UInt32 => "u32".into(),
        MavType::UInt64 => "u64".into(),
        MavType::Int8 => "i8".into(),
        MavType::Int16 => "i16".into(),
        MavType::Int32 => "i32".into(),
        MavType::Int64 => "i64".into(),
        MavType::Float => "f32".into(),
        MavType::Double => "f64".into(),
        MavType::Char => "char".into(),
        MavType::Array(inner, length) => format!("{}[{length}]", type_of(inner)),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

    use super::*;

    #[test]
    fn the_table_is_in_order_and_every_field_reads() {
        let messages = messages();
        assert!(messages.len() > 250, "{} messages", messages.len());
        assert!(messages.windows(2).all(|w| w[0].id() < w[1].id()));
        for message in messages {
            let tokens = message.3.split(' ').filter(|t| *t != "|").count();
            assert_eq!(message.fields().count(), tokens, "{}", message.name());
            let size: usize = message.fields().map(|f| f.size()).sum();
            assert!(size <= 255, "{}", message.name());
        }
    }

    #[test]
    fn a_heartbeat_and_a_trimmed_status_text() {
        let mut fake = crate::sim::FakeVehicle::new(1, 0xABCD);
        let bytes = fake.heartbeat();
        let frame = &crate::framing::split(&bytes).frames[0];
        let payload = crate::framing::payload(frame).unwrap();
        let decoded = decode(0, payload).unwrap();
        assert_eq!(decoded.name, "HEARTBEAT");
        assert_eq!(decoded.dialect, Dialect::Common);
        assert_eq!(decoded.fields["type"], 2, "a quadrotor");
        assert_eq!(decoded.fields["autopilot"], 3, "ArduPilot");
        assert_eq!(decoded.fields["mavlink_version"], 3);

        // STATUSTEXT as MAVLink 2 sends it: the zeros after the text are
        // trimmed, and with them the extensions.
        let mut payload = vec![4];
        payload.extend_from_slice(b"PreArm: Need Position Estimate");
        let decoded = decode(253, &payload).unwrap();
        assert_eq!(decoded.fields["severity"], 4);
        assert_eq!(decoded.fields["text"], "PreArm: Need Position Estimate");
        assert_eq!(decoded.fields["id"], 0);
        assert_eq!(decoded.fields["chunk_seq"], 0);

        let meminfo = decode(152, &[]).unwrap();
        assert_eq!(meminfo.name, "MEMINFO");
        assert_eq!(meminfo.dialect, Dialect::Ardupilotmega);
        assert!(decode(16_777_000, &[]).is_none());
    }

    #[test]
    fn arrays_large_numbers_and_not_a_number() {
        // ATTITUDE_QUATERNION_COV (61): time_usec u64, q f32[4], rollspeed,
        // pitchspeed, yawspeed f32, covariance f32[9].
        let mut payload = u64::MAX.to_le_bytes().to_vec();
        for q in [1.0_f32, 0.0, 0.0, 0.5] {
            payload.extend_from_slice(&q.to_le_bytes());
        }
        payload.extend_from_slice(&f32::NAN.to_le_bytes());
        let decoded = decode(61, &payload).unwrap();
        assert_eq!(decoded.fields["time_usec"], u64::MAX);
        assert_eq!(decoded.fields["q"], serde_json::json!([1.0, 0.0, 0.0, 0.5]));
        assert_eq!(decoded.fields["rollspeed"], Value::Null);
        assert_eq!(decoded.fields["covariance"].as_array().unwrap().len(), 9);
    }

    /// The checked-in table is what mavio's definitions give now.
    #[cfg(feature = "named")]
    #[test]
    fn the_table_matches_mavios_definitions() {
        assert!(
            // A Windows checkout may have CRLF line endings.
            generate() == include_str!("dialect/ardupilotmega.rs").replace("\r\n", "\n"),
            "the dialect table is out of date: run `cargo xtask mavlink-dialect`"
        );
    }

    /// For every message, random payloads of every length decode as the
    /// whole definitions decode them, field for field and in the same
    /// order.
    #[cfg(feature = "named")]
    #[test]
    fn random_payloads_decode_as_the_whole_definitions_do() {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut random = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for message in messages() {
            let size: usize = message.fields().map(|f| f.size()).sum();
            for length in 0..=size + 2 {
                for _ in 0..3 {
                    let payload: Vec<u8> = (0..length).map(|_| random().to_le_bytes()[0]).collect();
                    let ours = decode(message.id(), &payload).unwrap();
                    let theirs = crate::named::decode(message.id(), &payload).unwrap();
                    assert_eq!(ours.name, theirs.name);
                    assert_eq!(
                        Some(ours.dialect.name()),
                        crate::named::dialect_of(message.id()),
                        "{}",
                        ours.name
                    );
                    let pairs = |fields: &Map<String, Value>| -> Vec<(String, Value)> {
                        fields.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
                    };
                    assert_eq!(pairs(&ours.fields), pairs(&theirs.fields), "{}", ours.name);
                }
            }
        }
    }
}

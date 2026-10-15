//! Every message of the ArduPilot dialect by MAVLink's own names (ADR-0006
//! X1, X3): a payload decoded to its fields, and fields encoded back to a
//! frame, from the dialect's definitions rather than by hand. Field names
//! and raw units are MAVLink's (`GLOBAL_POSITION_INT.relative_alt` in mm),
//! so plugins read the MAVLink documentation, not ours.
//!
//! Plugins are this module's only user, and they run in the installed
//! daemon, so it is behind the `named` feature and stays out of the
//! browser's build.
//!
//! A plugin may bring its vendor's messages as a dialect file (S60): MAVLink's
//! XML, read here at run time, whose messages are then decoded and encoded
//! like the built-in ones while the plugin is loaded. A message whose id or
//! name the ArduPilot dialect or another plugin's dialect already has is
//! refused.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, PoisonError, RwLock};

use mavinspect::protocol::builders::{MessageBuilder, MessageFieldBuilder};
use mavinspect::protocol::{Dialect, MavType, MessageField, Units};

/// A message's definition, as a dialect file declares it.
pub use mavinspect::protocol::Message as Definition;
use mavinspect::utils::Builder as _;
use serde_json::{Map, Number, Value};

/// The dialect plugins speak: ArduPilot's, which includes the common set.
const DIALECT: &str = "ardupilotmega";

struct Index {
    by_id: HashMap<u32, &'static Definition>,
    by_name: HashMap<&'static str, &'static Definition>,
}

static INDEX: LazyLock<Index> = LazyLock::new(|| {
    let mut index = Index {
        by_id: HashMap::new(),
        by_name: HashMap::new(),
    };
    if let Some(dialect) = dialect() {
        for message in dialect.messages() {
            index.by_id.insert(message.id(), message);
            index.by_name.insert(message.name(), message);
        }
    }
    index
});

fn dialect() -> Option<&'static Dialect> {
    mavio::definitions::protocol().get_dialect_by_name(DIALECT)
}

/// Vendor messages from plugins' dialect files, each with the plugin that
/// brought it.
#[derive(Default)]
struct Vendor {
    by_id: HashMap<u32, (String, Arc<Definition>)>,
    by_name: HashMap<String, u32>,
}

static VENDOR: LazyLock<RwLock<Vendor>> = LazyLock::new(RwLock::default);

/// A message by id: the dialect's, else a plugin's.
fn by_id<T>(id: u32, f: impl FnOnce(&Definition) -> T) -> Option<T> {
    if let Some(message) = INDEX.by_id.get(&id) {
        return Some(f(message));
    }
    let vendor = VENDOR.read().unwrap_or_else(PoisonError::into_inner);
    vendor.by_id.get(&id).map(|(_, message)| f(message))
}

/// A message by name: the dialect's, else a plugin's.
fn by_name<T>(name: &str, f: impl FnOnce(&Definition) -> T) -> Option<T> {
    if let Some(message) = INDEX.by_name.get(name) {
        return Some(f(message));
    }
    let vendor = VENDOR.read().unwrap_or_else(PoisonError::into_inner);
    let id = vendor.by_name.get(name)?;
    vendor.by_id.get(id).map(|(_, message)| f(message))
}

/// The messages of a dialect file: MAVLink's XML, of which only
/// `<messages>` is read. Includes and enums do not change how a message is
/// laid out, so they are not needed and not followed.
///
/// # Errors
/// XML that does not parse, or a message without an id, a name or valid
/// field types, or too large for a frame.
pub fn parse_dialect(xml: &str) -> Result<Vec<Definition>, String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut messages = Vec::new();
    let mut open: Option<Open> = None;
    let mut text = String::new();
    loop {
        let event = reader
            .read_event()
            .map_err(|e| format!("the dialect is not XML: {e}"))?;
        match event {
            Event::Start(tag) | Event::Empty(tag) if tag.name().as_ref() == b"message" => {
                text.clear();
                open = Some(Open::new(&tag)?);
            }
            Event::Empty(tag) if tag.name().as_ref() == b"extensions" => {
                if let Some(message) = open.as_mut() {
                    message.extension = true;
                }
            }
            Event::Start(tag) | Event::Empty(tag) if tag.name().as_ref() == b"field" => {
                text.clear();
                if let Some(message) = open.as_mut() {
                    message.field(&tag)?;
                }
            }
            Event::Start(_) => text.clear(),
            Event::Text(t) => text.push_str(&t.unescape().unwrap_or_default()),
            Event::End(tag) => {
                let Some(message) = open.as_mut() else {
                    continue;
                };
                match tag.name().as_ref() {
                    b"description" if message.description.is_empty() => {
                        text.trim().clone_into(&mut message.description);
                    }
                    // A field's text is its description.
                    b"field" => {
                        if let Some(last) = message.fields.last_mut() {
                            text.trim().clone_into(&mut last.description);
                        }
                    }
                    b"message" => {
                        if let Some(message) = open.take() {
                            messages.push(message.build()?);
                        }
                    }
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(messages)
}

fn attribute(tag: &quick_xml::events::BytesStart, key: &str) -> Option<String> {
    tag.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == key.as_bytes())
        .and_then(|a| a.unescape_value().ok())
        .map(|v| v.trim().to_owned())
}

/// A message being read from a dialect file.
struct Open {
    id: u32,
    name: String,
    description: String,
    fields: Vec<OpenField>,
    /// Past `<extensions/>`.
    extension: bool,
}

struct OpenField {
    name: String,
    kind: MavType,
    units: Option<Units>,
    r#enum: Option<String>,
    extension: bool,
    description: String,
}

impl Open {
    fn new(tag: &quick_xml::events::BytesStart) -> Result<Self, String> {
        let name = attribute(tag, "name").unwrap_or_default();
        let id = attribute(tag, "id")
            .and_then(|id| id.parse::<u32>().ok())
            .filter(|id| *id < 1 << 24)
            .ok_or_else(|| format!("message {name:?} has no id from 0 to 16777215"))?;
        let valid = !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
        if !valid {
            return Err(format!(
                "message {id}'s name {name:?} must be capitals, digits and underscores"
            ));
        }
        Ok(Self {
            id,
            name,
            description: String::new(),
            fields: Vec::new(),
            extension: false,
        })
    }

    fn field(&mut self, tag: &quick_xml::events::BytesStart) -> Result<(), String> {
        let name = attribute(tag, "name").unwrap_or_default();
        let kind = attribute(tag, "type").unwrap_or_default();
        let kind = MavType::parse(&kind)
            .map_err(|_| format!("{}.{name}: no MAVLink type {kind:?}", self.name))?;
        if name.is_empty() {
            return Err(format!("{}: a field has no name", self.name));
        }
        self.fields.push(OpenField {
            name,
            kind,
            units: attribute(tag, "units").and_then(|u| Units::parse(&u).ok()),
            r#enum: attribute(tag, "enum"),
            extension: self.extension,
            description: String::new(),
        });
        Ok(())
    }

    fn build(self) -> Result<Definition, String> {
        let fields = self
            .fields
            .into_iter()
            .map(|f| {
                MessageFieldBuilder::new()
                    .set_name(&f.name)
                    .set_type(f.kind)
                    .set_units(f.units)
                    .set_enum(f.r#enum)
                    .set_extension(f.extension)
                    .set_description(&f.description)
                    .build()
            })
            .collect();
        build(self.id, &self.name, &self.description, fields)
    }
}

fn build(
    id: u32,
    name: &str,
    description: &str,
    fields: Vec<MessageField>,
) -> Result<Definition, String> {
    if fields.is_empty() || fields.len() > 64 {
        return Err(format!("{name}: a message has 1 to 64 fields"));
    }
    let message = MessageBuilder::new()
        .set_id(id)
        .set_name(name)
        .set_description(description)
        .set_fields(fields)
        .set_defined_in("vendor")
        .build();
    if message.size_v2() > 255 {
        return Err(format!(
            "{name}: {} bytes, more than a frame's 255",
            message.size_v2()
        ));
    }
    Ok(message)
}

/// Whether a plugin's messages can be added: none may share an id or a name
/// with the ArduPilot dialect, with another plugin's, or with each other.
///
/// # Errors
/// The first clash, in words.
pub fn check(owner: &str, messages: &[Definition]) -> Result<(), String> {
    let vendor = VENDOR.read().unwrap_or_else(PoisonError::into_inner);
    let mut seen_ids = std::collections::HashSet::new();
    let mut seen_names = std::collections::HashSet::new();
    for message in messages {
        let (id, name) = (message.id(), message.name());
        if let Some(theirs) = INDEX.by_id.get(&id) {
            return Err(format!(
                "{name} has id {id}, which is {} in the ArduPilot dialect",
                theirs.name()
            ));
        }
        if INDEX.by_name.contains_key(name) {
            return Err(format!(
                "{name} is already a message of the ArduPilot dialect"
            ));
        }
        let other = |found: Option<&(String, Arc<Definition>)>| {
            found.filter(|(who, _)| who != owner).cloned()
        };
        if let Some((who, theirs)) = other(vendor.by_id.get(&id)) {
            return Err(format!(
                "{name} has id {id}, which is {} in the {who} plugin's dialect",
                theirs.name()
            ));
        }
        if let Some((who, _)) = other(vendor.by_name.get(name).and_then(|id| vendor.by_id.get(id)))
        {
            return Err(format!(
                "{name} is already a message of the {who} plugin's dialect"
            ));
        }
        if !seen_ids.insert(id) || !seen_names.insert(name) {
            return Err(format!("{name} ({id}) appears twice in the dialect"));
        }
    }
    Ok(())
}

/// Adds a plugin's messages, replacing any it brought before.
///
/// # Errors
/// As [`check`].
pub fn register(owner: &str, messages: Vec<Definition>) -> Result<(), String> {
    check(owner, &messages)?;
    unregister(owner);
    let mut vendor = VENDOR.write().unwrap_or_else(PoisonError::into_inner);
    for message in messages {
        vendor
            .by_name
            .insert(message.name().to_owned(), message.id());
        vendor
            .by_id
            .insert(message.id(), (owner.to_owned(), Arc::new(message)));
    }
    Ok(())
}

/// Removes a plugin's messages.
pub fn unregister(owner: &str) {
    let mut vendor = VENDOR.write().unwrap_or_else(PoisonError::into_inner);
    vendor.by_id.retain(|_, (who, _)| who != owner);
    let ids: std::collections::HashSet<u32> = vendor.by_id.keys().copied().collect();
    vendor.by_name.retain(|_, id| ids.contains(id));
}

/// One field of a message, for the SDK's types and for people.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldInfo {
    pub name: String,
    /// MAVLink's type, such as `int32_t` or `char[16]`.
    pub kind: String,
    /// MAVLink's unit, such as `mm` or `degE7`, if it has one.
    pub unit: Option<String>,
    pub description: String,
}

/// Every message name in the ArduPilot dialect, sorted.
#[must_use]
pub fn names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = INDEX.by_name.keys().copied().collect();
    names.sort_unstable();
    names
}

#[must_use]
pub fn id_of(name: &str) -> Option<u32> {
    by_name(name, Definition::id)
}

#[must_use]
pub fn name_of(id: u32) -> Option<String> {
    by_id(id, |m| m.name().to_owned())
}

/// Ids of MAVLink's common dialect, which ArduPilot's includes.
static COMMON: LazyLock<std::collections::HashSet<u32>> = LazyLock::new(|| {
    mavio::definitions::protocol()
        .get_dialect_by_name("common")
        .map(|common| common.messages().into_iter().map(Definition::id).collect())
        .unwrap_or_default()
});

/// The dialect whose page on mavlink.io documents a message: `common`, or
/// `ardupilotmega` for ArduPilot's own. None for a plugin's or an unknown
/// one.
#[must_use]
pub fn dialect_of(id: u32) -> Option<&'static str> {
    if COMMON.contains(&id) {
        Some("common")
    } else if INDEX.by_id.contains_key(&id) {
        Some(DIALECT)
    } else {
        None
    }
}

/// A message's fields in declared order, with their units.
#[must_use]
pub fn fields(name: &str) -> Option<Vec<FieldInfo>> {
    by_name(name, |message| {
        message
            .fields()
            .iter()
            .map(|field| FieldInfo {
                name: field.name().to_owned(),
                kind: type_name(field.r#type()),
                unit: field.units().map(ToString::to_string),
                description: field.description().trim().to_owned(),
            })
            .collect()
    })
}

fn type_name(kind: &MavType) -> String {
    match kind {
        MavType::UInt8 => "uint8_t".into(),
        MavType::UInt16 => "uint16_t".into(),
        MavType::UInt32 => "uint32_t".into(),
        MavType::UInt64 => "uint64_t".into(),
        MavType::Int8 => "int8_t".into(),
        MavType::Int16 => "int16_t".into(),
        MavType::Int32 => "int32_t".into(),
        MavType::Int64 => "int64_t".into(),
        MavType::Float => "float".into(),
        MavType::Double => "double".into(),
        MavType::Char => "char".into(),
        MavType::UInt8MavlinkVersion => "uint8_t_mavlink_version".into(),
        MavType::Array(inner, length) => format!("{}[{length}]", type_name(inner)),
    }
}

/// A decoded message: its name and its fields by MAVLink's names.
#[derive(Clone, Debug, PartialEq)]
pub struct Decoded {
    pub name: String,
    pub fields: Map<String, Value>,
}

/// Decodes a payload. MAVLink 2 trims trailing zero bytes; they are put
/// back. None for an id the dialect does not have.
#[must_use]
pub fn decode(id: u32, payload: &[u8]) -> Option<Decoded> {
    by_id(id, |message| decode_with(message, payload))
}

fn decode_with(message: &Definition, payload: &[u8]) -> Decoded {
    let mut bytes = payload.to_vec();
    bytes.resize(message.size_v2().max(payload.len()), 0);
    let mut at = 0;
    let mut fields = Map::new();
    for field in message.reordered_fields() {
        let size = field.r#type().size();
        let value = read(field.r#type(), &bytes[at..at + size]);
        fields.insert(field.name().to_owned(), value);
        at += size;
    }
    Decoded {
        name: message.name().to_owned(),
        fields,
    }
}

fn read(kind: &MavType, bytes: &[u8]) -> Value {
    match kind {
        MavType::Array(inner, _) if **inner == MavType::Char => {
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            Value::String(String::from_utf8_lossy(&bytes[..end]).into_owned())
        }
        MavType::Array(inner, _) => {
            Value::Array(bytes.chunks(inner.size()).map(|c| read(inner, c)).collect())
        }
        scalar => read_scalar(scalar, bytes),
    }
}

fn read_scalar(kind: &MavType, b: &[u8]) -> Value {
    let array = |n: usize| -> [u8; 8] {
        let mut out = [0u8; 8];
        out[..n].copy_from_slice(&b[..n]);
        out
    };
    match kind {
        MavType::UInt8 | MavType::UInt8MavlinkVersion | MavType::Char => Value::from(b[0]),
        MavType::Int8 => Value::from(i8::from_le_bytes([b[0]])),
        MavType::UInt16 => Value::from(u16::from_le_bytes([b[0], b[1]])),
        MavType::Int16 => Value::from(i16::from_le_bytes([b[0], b[1]])),
        MavType::UInt32 => Value::from(u32::from_le_bytes([b[0], b[1], b[2], b[3]])),
        MavType::Int32 => Value::from(i32::from_le_bytes([b[0], b[1], b[2], b[3]])),
        MavType::UInt64 => Value::from(u64::from_le_bytes(array(8))),
        MavType::Int64 => Value::from(i64::from_le_bytes(array(8))),
        // NaN and infinities have no JSON number: null, as MAVLink often
        // means "unknown" by NaN.
        MavType::Float => Number::from_f64(f64::from(f32::from_le_bytes([b[0], b[1], b[2], b[3]])))
            .map_or(Value::Null, Value::Number),
        MavType::Double => {
            Number::from_f64(f64::from_le_bytes(array(8))).map_or(Value::Null, Value::Number)
        }
        MavType::Array(..) => Value::Null,
    }
}

/// A message ready for a frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Encoded {
    pub id: u32,
    pub crc_extra: u8,
    /// The whole payload; the frame trims trailing zeros.
    pub payload: Vec<u8>,
}

/// Encodes a message by name. Fields left out are zero; a field the
/// message does not have, or a value that does not fit, is an error that
/// says which.
///
/// # Errors
///
/// An unknown message or field, or a value of the wrong kind or size.
pub fn encode(name: &str, fields: &Map<String, Value>) -> Result<Encoded, String> {
    by_name(name, |message| encode_with(message, fields)).unwrap_or_else(|| {
        Err(format!(
            "no message {name} in the ArduPilot dialect or a plugin's"
        ))
    })
}

fn encode_with(message: &Definition, fields: &Map<String, Value>) -> Result<Encoded, String> {
    let name = message.name();
    if let Some(unknown) = fields
        .keys()
        .find(|key| message.get_field_by_name(key.as_str()).is_none())
    {
        return Err(format!("{name} has no field {unknown}"));
    }
    let mut payload = Vec::with_capacity(message.size_v2());
    for field in message.reordered_fields() {
        let value = fields.get(field.name()).unwrap_or(&Value::Null);
        write(field.r#type(), value, &mut payload)
            .map_err(|why| format!("{name}.{}: {why}", field.name()))?;
    }
    Ok(Encoded {
        id: message.id(),
        crc_extra: message.crc_extra(),
        payload,
    })
}

fn write(kind: &MavType, value: &Value, out: &mut Vec<u8>) -> Result<(), String> {
    match kind {
        MavType::Array(inner, length) if **inner == MavType::Char => {
            let text = match value {
                Value::Null => "",
                Value::String(text) => text.as_str(),
                _ => return Err("expected text".into()),
            };
            if text.len() > *length {
                return Err(format!("at most {length} bytes"));
            }
            out.extend_from_slice(text.as_bytes());
            out.resize(out.len() + length - text.len(), 0);
            Ok(())
        }
        MavType::Array(inner, length) => {
            let items = match value {
                Value::Null => &[][..],
                Value::Array(items) => items.as_slice(),
                _ => return Err("expected a list".into()),
            };
            if items.len() > *length {
                return Err(format!("at most {length} values"));
            }
            for n in 0..*length {
                write(inner, items.get(n).unwrap_or(&Value::Null), out)?;
            }
            Ok(())
        }
        scalar => write_scalar(scalar, value, out),
    }
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "f32 fields are sent as f32, as MAVLink defines them"
)]
fn write_scalar(kind: &MavType, value: &Value, out: &mut Vec<u8>) -> Result<(), String> {
    let float = || -> Result<f64, String> {
        match value {
            Value::Null => Ok(0.0),
            Value::Number(n) => n.as_f64().ok_or_else(|| "expected a number".into()),
            _ => Err("expected a number".into()),
        }
    };
    let signed = || -> Result<i64, String> {
        match value {
            Value::Null => Ok(0),
            Value::Bool(b) => Ok(i64::from(*b)),
            Value::Number(n) => n.as_i64().ok_or_else(|| "expected a whole number".into()),
            _ => Err("expected a whole number".into()),
        }
    };
    let unsigned = || -> Result<u64, String> {
        match value {
            Value::Null => Ok(0),
            Value::Bool(b) => Ok(u64::from(*b)),
            Value::Number(n) => n
                .as_u64()
                .ok_or_else(|| "expected a whole number, not negative".into()),
            _ => Err("expected a whole number".into()),
        }
    };
    let fits = || "out of range".to_owned();
    match kind {
        MavType::UInt8 | MavType::UInt8MavlinkVersion | MavType::Char => {
            out.push(u8::try_from(unsigned()?).map_err(|_| fits())?);
        }
        MavType::Int8 => {
            out.extend(i8::try_from(signed()?).map_err(|_| fits())?.to_le_bytes());
        }
        MavType::UInt16 => {
            out.extend(
                u16::try_from(unsigned()?)
                    .map_err(|_| fits())?
                    .to_le_bytes(),
            );
        }
        MavType::Int16 => {
            out.extend(i16::try_from(signed()?).map_err(|_| fits())?.to_le_bytes());
        }
        MavType::UInt32 => {
            out.extend(
                u32::try_from(unsigned()?)
                    .map_err(|_| fits())?
                    .to_le_bytes(),
            );
        }
        MavType::Int32 => {
            out.extend(i32::try_from(signed()?).map_err(|_| fits())?.to_le_bytes());
        }
        MavType::UInt64 => out.extend(unsigned()?.to_le_bytes()),
        MavType::Int64 => out.extend(signed()?.to_le_bytes()),
        MavType::Float => out.extend((float()? as f32).to_le_bytes()),
        MavType::Double => out.extend(float()?.to_le_bytes()),
        MavType::Array(..) => return Err("nested lists".into()),
    }
    Ok(())
}

/// A whole MAVLink 2 frame for an encoded message.
#[must_use]
pub fn frame(encoded: &Encoded, system_id: u8, component_id: u8, sequence: u8) -> Vec<u8> {
    use mavio::Frame;
    use mavio::protocol::{V2, Versionless};
    let frame: Frame<Versionless> = Frame::builder()
        .version(V2)
        .system_id(system_id)
        .component_id(component_id)
        .sequence(sequence)
        .message_id(encoded.id)
        .payload(&encoded.payload)
        .crc_extra(encoded.crc_extra)
        .build()
        .into_versionless();
    let mut buf = vec![0u8; frame.size()];
    let written = frame.serialize(&mut buf).unwrap_or(0);
    buf.truncate(written);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use mavinspect::protocol::MessageField;

    /// A value for every field that survives the trip exactly.
    fn sample(kind: &MavType, n: usize) -> Value {
        let k = i64::try_from(n % 100).unwrap_or(0);
        match kind {
            MavType::Array(inner, length) if **inner == MavType::Char => Value::String(
                "ab".repeat(*length / 2)
                    .chars()
                    .take(length.saturating_sub(1))
                    .collect(),
            ),
            MavType::Array(inner, length) => {
                Value::Array((0..*length).map(|i| sample(inner, n + i)).collect())
            }
            MavType::UInt8 | MavType::UInt8MavlinkVersion | MavType::Char => Value::from(k + 1),
            MavType::Int8 => Value::from(-k),
            MavType::UInt16 | MavType::UInt32 | MavType::UInt64 => Value::from(1000 + k),
            MavType::Int16 | MavType::Int32 | MavType::Int64 => Value::from(-1000 - k),
            MavType::Float | MavType::Double => {
                Value::from(0.5 + f64::from(u8::try_from(k).unwrap_or(0)))
            }
        }
    }

    #[test]
    fn common_messages_are_told_from_ardupilots() {
        assert_eq!(dialect_of(0), Some("common"), "HEARTBEAT");
        assert_eq!(dialect_of(33), Some("common"), "GLOBAL_POSITION_INT");
        assert_eq!(dialect_of(152), Some("ardupilotmega"), "MEMINFO");
        assert_eq!(dialect_of(16_777_000), None);
    }

    #[test]
    fn every_message_of_the_dialect_encodes_and_decodes_back_the_same() {
        let names = names();
        assert!(names.len() > 250, "{} messages", names.len());
        for name in names {
            let message = INDEX.by_name[name];
            // An enum's field takes one of its entries, which the typed
            // decoder checks; bitmasks take 0.
            let entry = |field: &MessageField| {
                let enum_name = field.r#enum()?;
                let found = dialect()?.get_enum_by_name(enum_name)?;
                if found.bitmask() || field.bitmask() {
                    return Some(0);
                }
                found
                    .entries()
                    .first()
                    .map(mavinspect::protocol::EnumEntry::value)
            };
            let fields: Map<String, Value> = message
                .fields()
                .iter()
                .enumerate()
                .map(|(n, f)| {
                    let value = match (entry(f), f.r#type()) {
                        (Some(v), MavType::Array(_, length)) => {
                            Value::Array(vec![Value::from(v); *length])
                        }
                        (Some(v), _) => Value::from(v),
                        (None, kind) => sample(kind, n),
                    };
                    (f.name().to_owned(), value)
                })
                .collect();
            let encoded = encode(name, &fields).unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(encoded.payload.len(), message.size_v2(), "{name}");
            let decoded = decode(encoded.id, &encoded.payload).expect(name);
            assert_eq!(decoded.name, name);
            assert_eq!(decoded.fields, fields, "{name}");
            // Through a whole frame, as a vehicle sends it.
            let bytes = frame(&encoded, 1, 1, 0);
            let split = crate::framing::split(&bytes);
            assert_eq!(split.frames.len(), 1, "{name}");
            // The checksum holds with mavio's own CRC_EXTRA, and its typed
            // decoder reads the same bytes.
            assert_eq!(
                crate::decode::check(&bytes),
                crate::framing::Check::Valid,
                "{name}"
            );
            assert!(typed(&bytes), "{name}");
        }
    }

    fn typed(bytes: &[u8]) -> bool {
        use mavio::dialects::Ardupilotmega;
        use mavio::protocol::Versionless;
        use mavio::{Receiver, io::StdIoReader};
        let mut receiver = Receiver::new::<Versionless>(StdIoReader::new(bytes));
        receiver
            .recv()
            .is_ok_and(|frame| frame.decode::<Ardupilotmega>().is_ok())
    }

    #[test]
    fn decoding_agrees_with_the_typed_messages() {
        let mut fake = crate::sim::FakeVehicle::new(1, 0xABCD);
        let bytes = fake.heartbeat();
        let frame = &crate::framing::split(&bytes).frames[0];
        let header = crate::framing::header(frame).expect("header");
        let decoded = decode(
            header.message_id,
            crate::framing::payload(frame).expect("payload"),
        )
        .expect("heartbeat");
        assert_eq!(decoded.name, "HEARTBEAT");
        assert_eq!(decoded.fields["type"], 2, "a quadrotor");
        assert_eq!(decoded.fields["autopilot"], 3, "ArduPilot");
        assert_eq!(decoded.fields["mavlink_version"], 3);

        let ack = crate::sim::command_ack_frame(1, 1, (255, 190), 0, 2000, 0);
        let frame = &crate::framing::split(&ack).frames[0];
        let decoded = decode(77, crate::framing::payload(frame).expect("payload")).expect("ack");
        assert_eq!(decoded.name, "COMMAND_ACK");
        assert_eq!(decoded.fields["command"], 2000);
        assert_eq!(decoded.fields["result"], 0);
    }

    #[test]
    fn units_names_and_mistakes() {
        let fields = fields("GLOBAL_POSITION_INT").expect("known");
        let relative = fields
            .iter()
            .find(|f| f.name == "relative_alt")
            .expect("field");
        assert_eq!(relative.unit.as_deref(), Some("mm"));
        assert_eq!(relative.kind, "int32_t");
        assert_eq!(id_of("GLOBAL_POSITION_INT"), Some(33));
        assert_eq!(name_of(33).as_deref(), Some("GLOBAL_POSITION_INT"));
        let mut wrong = Map::new();
        wrong.insert("altitude".into(), Value::from(1));
        assert_eq!(
            encode("GLOBAL_POSITION_INT", &wrong),
            Err("GLOBAL_POSITION_INT has no field altitude".into())
        );
        let mut big = Map::new();
        big.insert("satellites_visible".into(), Value::from(300));
        assert!(
            encode("GPS_RAW_INT", &big)
                .unwrap_err()
                .contains("out of range")
        );
        assert!(encode("NOT_A_MESSAGE", &Map::new()).is_err());
    }

    /// A message as a dialect file would declare it.
    fn xml_of(message: &Definition) -> String {
        use std::fmt::Write as _;
        let mut out = format!(
            "<message id=\"{}\" name=\"{}\"><description>d</description>",
            message.id(),
            message.name()
        );
        let mut extended = false;
        for field in message.fields() {
            if field.extension() && !extended {
                out.push_str("<extensions/>");
                extended = true;
            }
            let _ = write!(
                out,
                "<field type=\"{}\" name=\"{}\">x</field>",
                type_name(field.r#type()),
                field.name()
            );
        }
        out + "</message>"
    }

    #[test]
    fn every_message_read_from_its_xml_is_laid_out_as_the_built_in_one() {
        let names = names();
        let xml = format!(
            "<mavlink><include>common.xml</include><messages>{}</messages></mavlink>",
            names
                .iter()
                .map(|name| xml_of(INDEX.by_name[name]))
                .collect::<String>()
        );
        let parsed = parse_dialect(&xml).expect("parses");
        assert_eq!(parsed.len(), names.len());
        for read in &parsed {
            let built = INDEX.by_name[read.name()];
            assert_eq!(read.id(), built.id(), "{}", read.name());
            assert_eq!(read.crc_extra(), built.crc_extra(), "{}", read.name());
            assert_eq!(read.size_v2(), built.size_v2(), "{}", read.name());
            let order = |m: &Definition| -> Vec<String> {
                m.reordered_fields()
                    .iter()
                    .map(|f| f.name().to_owned())
                    .collect()
            };
            assert_eq!(order(read), order(built), "{}", read.name());
            // The same bytes decode the same either way.
            let fields: Map<String, Value> = built
                .fields()
                .iter()
                .enumerate()
                .map(|(n, f)| (f.name().to_owned(), sample(f.r#type(), n)))
                .collect();
            let encoded = encode_with(built, &fields).expect("encodes");
            assert_eq!(
                decode_with(read, &encoded.payload),
                decode_with(built, &encoded.payload),
                "{}",
                read.name()
            );
        }
    }

    #[test]
    fn a_vendor_dialect_is_decoded_and_encoded_back_the_same() {
        let messages = parse_dialect(include_str!("../fixtures/vendor_test.xml")).expect("parses");
        assert_eq!(messages.len(), 2);
        register("acme-gimbal", messages).expect("no clash");
        assert_eq!(id_of("ACME_GIMBAL_STATUS"), Some(46_000));
        assert_eq!(name_of(46_001).as_deref(), Some("ACME_GIMBAL_POINT"));
        let info = fields("ACME_GIMBAL_STATUS").expect("known");
        assert_eq!(info[1].unit.as_deref(), Some("deg"));
        assert_eq!(info[1].description, "Pan angle.");

        let mut status = Map::new();
        status.insert("time_usec".into(), Value::from(123_456_u64));
        status.insert("pan".into(), Value::from(12.5));
        status.insert("tilt".into(), Value::from(-30.0));
        status.insert("mode".into(), Value::from(1));
        status.insert("serial".into(), Value::from("AC-0042"));
        status.insert("temperature".into(), Value::from(-250));
        let encoded = encode("ACME_GIMBAL_STATUS", &status).expect("encodes");
        assert_eq!(encoded.id, 46_000);
        let decoded = decode(46_000, &encoded.payload).expect("decodes");
        assert_eq!(decoded.name, "ACME_GIMBAL_STATUS");
        assert_eq!(decoded.fields, status);
        // Through a whole frame, trimmed as MAVLink 2 sends it.
        let bytes = frame(&encoded, 1, 154, 7);
        let split = crate::framing::split(&bytes);
        let payload = crate::framing::payload(split.frames[0]).expect("payload");
        assert_eq!(decode(46_000, payload).expect("decodes").fields, status);

        unregister("acme-gimbal");
        assert_eq!(id_of("ACME_GIMBAL_STATUS"), None);
        assert!(decode(46_000, &encoded.payload).is_none());
    }

    #[test]
    fn a_vendor_message_may_not_take_an_id_or_a_name_already_used() {
        let one = |id: u32, name: &str| {
            parse_dialect(&format!(
                "<mavlink><messages><message id=\"{id}\" name=\"{name}\"><field type=\"uint8_t\" name=\"a\"/></message></messages></mavlink>"
            ))
            .expect("parses")
        };
        assert_eq!(
            check("p", &one(0, "MY_BEAT")),
            Err("MY_BEAT has id 0, which is HEARTBEAT in the ArduPilot dialect".into())
        );
        assert_eq!(
            check("p", &one(43_000, "HEARTBEAT")),
            Err("HEARTBEAT is already a message of the ArduPilot dialect".into())
        );
        register("first", one(43_001, "FIRST_THING")).expect("no clash");
        assert_eq!(
            check("second", &one(43_001, "SECOND_THING")),
            Err(
                "SECOND_THING has id 43001, which is FIRST_THING in the first plugin's dialect"
                    .into()
            )
        );
        assert_eq!(
            check("second", &one(43_002, "FIRST_THING")),
            Err("FIRST_THING is already a message of the first plugin's dialect".into())
        );
        // The same plugin loading again replaces its own.
        register("first", one(43_001, "FIRST_THING")).expect("its own");
        unregister("first");
        assert!(
            parse_dialect("<mavlink><messages><message name=\"X\"/></messages></mavlink>").is_err()
        );
        assert!(
            parse_dialect(
                "<mavlink><messages><message id=\"1\" name=\"bad name\"/></messages></mavlink>"
            )
            .is_err()
        );
    }
}

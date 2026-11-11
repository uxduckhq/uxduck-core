//! A fork's own MAVLink messages at run time (ADR-0044). Dialect files a
//! person adds are read here into the records [`uxduck_core::domain::dialect`]
//! keeps: each message compared with UXDuck's built-in ArduPilot table
//! ([`crate::dialect`]) and kept only when it is new or differs, with its
//! `CRC_EXTRA` and the enums its fields name. Nothing here needs mavio's
//! definitions, so UXDuck Web has it too.
//!
//! The engine publishes which messages describe each vehicle
//! ([`publish`]), and the inspector, `mavlink/history` and plugins decode
//! a vehicle's frames with them first ([`for_vehicle`]). The reassembler
//! of byte streams accepts a frame whose checksum matches any added
//! dialect's definition of its id ([`checksum_matches`]), so a changed
//! message is not thrown away as noise. UXDuck's own readers
//! ([`crate::decode`]) are unchanged: they read the built-in definitions
//! only (F5).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, PoisonError, RwLock};

use serde_json::{Map, Value};
use uxduck_core::domain::dialect::{
    CommandParam, CustomCommand, CustomEnum, CustomField, CustomMessage, Dialect, EnumEntry,
    FieldKind, MAX_COMMANDS, MAX_ENTRIES, MAX_ENUMS, MAX_MESSAGES, MessageChange,
};

use mavio::dialects::ardupilotmega::enums::MavCmd;

use crate::dialect::Kind;

/// What a dialect's files hold that the built-in dialect does not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Read {
    pub messages: Vec<CustomMessage>,
    pub enums: Vec<CustomEnum>,
    /// `MAV_CMD` entries whose numbers the built-in dialect does not have.
    pub commands: Vec<CustomCommand>,
    /// Messages the same as the built-in dialect's, not kept.
    pub unchanged: u32,
}

/// Reads dialect files, given as their names and texts, and keeps the
/// messages that are new or differ from the built-in dialect, with the
/// enums those messages' fields name, and the `MAV_CMD` entries whose
/// numbers it does not have (F14). `<include>` is not followed: give the
/// included files too.
///
/// # Errors
/// XML that does not parse; a message without a valid id, name or fields,
/// or too large for a frame; an id or a name given twice; more than
/// [`MAX_MESSAGES`] kept, more than [`MAX_ENUMS`] enums or more than
/// [`MAX_COMMANDS`] commands; or nothing that differs from the built-in
/// dialect.
pub fn read(files: &[(&str, &str)]) -> Result<Read, String> {
    if files.is_empty() {
        return Err("give at least one dialect file".into());
    }
    let mut messages: Vec<CustomMessage> = Vec::new();
    let mut enums: BTreeMap<String, CustomEnum> = BTreeMap::new();
    let mut ids: HashMap<u32, String> = HashMap::new();
    let mut names: HashMap<String, u32> = HashMap::new();
    let mut commands: BTreeMap<u16, CustomCommand> = BTreeMap::new();
    for (file, text) in files {
        let parsed = parse(text).map_err(|e| format!("{file}: {e}"))?;
        commands.extend(parsed.commands.into_iter().map(|c| (c.value, c)));
        for message in parsed.messages {
            if let Some(other) = ids.get(&message.id) {
                return Err(format!(
                    "{file}: {} has id {}, which {other} has already",
                    message.name, message.id
                ));
            }
            if let Some(other) = names.get(&message.name) {
                return Err(format!(
                    "{file}: {} is given twice, as {other} and {}",
                    message.name, message.id
                ));
            }
            ids.insert(message.id, message.name.clone());
            names.insert(message.name.clone(), message.id);
            messages.push(message);
        }
        for found in parsed.enums {
            let kept = enums
                .entry(found.name.clone())
                .or_insert_with(|| CustomEnum {
                    name: found.name.clone(),
                    bitmask: false,
                    entries: Vec::new(),
                });
            kept.bitmask |= found.bitmask;
            for entry in found.entries {
                kept.entries
                    .retain(|e| e.value != entry.value && e.name != entry.name);
                kept.entries.push(entry);
            }
        }
    }
    let total = messages.len();
    let mut kept: Vec<CustomMessage> = messages
        .into_iter()
        .filter_map(|mut message| {
            message.change = compare(&message)?;
            Some(message)
        })
        .collect();
    if kept.len() > MAX_MESSAGES {
        return Err(format!(
            "{} messages are new or changed, more than the {MAX_MESSAGES} a dialect may keep",
            kept.len()
        ));
    }
    kept.sort_by_key(|m| m.id);
    let commands = new_commands(commands)?;
    if kept.is_empty() && commands.is_empty() {
        return Err(if total == 0 {
            "the files have no messages and no commands UXDuck does not know".into()
        } else {
            format!(
                "all {total} messages are the same as UXDuck's ArduPilot dialect: nothing to add"
            )
        });
    }
    let enum_names: std::collections::BTreeSet<&str> = kept
        .iter()
        .flat_map(|m| m.fields.iter().filter_map(|f| f.enum_name.as_deref()))
        .collect();
    let mut needed: Vec<CustomEnum> = enums
        .into_values()
        .filter(|e| enum_names.contains(e.name.as_str()) && !e.entries.is_empty())
        .collect();
    if needed.len() > MAX_ENUMS {
        return Err(format!(
            "the messages name {} enums, more than the {MAX_ENUMS} a dialect may keep",
            needed.len()
        ));
    }
    for found in &mut needed {
        if found.entries.len() > MAX_ENTRIES {
            return Err(format!(
                "{} has {} entries, more than {MAX_ENTRIES}",
                found.name,
                found.entries.len()
            ));
        }
        found.entries.sort_by_key(|e| e.value);
    }
    let unchanged = u32::try_from(total - kept.len()).unwrap_or(u32::MAX);
    Ok(Read {
        messages: kept,
        enums: needed,
        commands,
        unchanged,
    })
}

/// The built-in numbers left for a fork to name with no place,
/// `MAV_CMD_USER_1` to `_5`. Only these may be said to have none, so a
/// dialect can't stop a waypoint's frame being checked on readback.
const PLACELESS_USER_COMMANDS: std::ops::RangeInclusive<u16> = 31_010..=31_014;

/// Whether a dialect may say this command has no place: a number the
/// built-in dialect lacks, or `MAV_CMD_USER_1` to `_5`.
#[must_use]
pub fn may_be_placeless(value: u16) -> bool {
    MavCmd::try_from(value).is_err() || PLACELESS_USER_COMMANDS.contains(&value)
}

/// The commands the built-in dialect does not have, by number.
fn new_commands(found: BTreeMap<u16, CustomCommand>) -> Result<Vec<CustomCommand>, String> {
    let commands: Vec<CustomCommand> = found
        .into_values()
        .filter(|c| !built_in_command(c))
        .map(|mut c| {
            if !may_be_placeless(c.value) {
                c.no_location = false;
            }
            c
        })
        .collect();
    if commands.len() > MAX_COMMANDS {
        return Err(format!(
            "{} commands are new, more than the {MAX_COMMANDS} a dialect may keep",
            commands.len()
        ));
    }
    Ok(commands)
}

/// Whether the built-in dialect has this command, by number and name: a
/// fork naming one of the `MAV_CMD_USER_1` numbers its own way is kept.
/// mavio names `MAV_CMD_WAYPOINT_USER_1` `WaypointUser1`.
fn built_in_command(command: &CustomCommand) -> bool {
    let Ok(known) = MavCmd::try_from(command.value) else {
        return false;
    };
    let variant: String = command
        .name
        .trim_start_matches("MAV_CMD_")
        .split('_')
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_string() + &chars.as_str().to_lowercase()
            })
        })
        .collect();
    format!("{known:?}") == variant
}

/// How a message differs from the built-in dialect's of the same id, or
/// None when it does not.
fn compare(message: &CustomMessage) -> Option<MessageChange> {
    let Some(built_in) = crate::dialect::message(message.id) else {
        return Some(MessageChange::New);
    };
    let theirs: Vec<(String, FieldKind, Option<u8>, bool)> = built_in
        .fields()
        .map(|f| {
            (
                f.name.to_owned(),
                field_kind(f.kind),
                f.length.and_then(|n| u8::try_from(n).ok()),
                f.extension,
            )
        })
        .collect();
    let ours: Vec<(String, FieldKind, Option<u8>, bool)> = message
        .fields
        .iter()
        .map(|f| (f.name.clone(), f.kind, f.length, f.extension))
        .collect();
    let base = |fields: &[(String, FieldKind, Option<u8>, bool)]| {
        let mut base: Vec<(String, FieldKind, Option<u8>)> = fields
            .iter()
            .filter(|f| !f.3)
            .map(|f| (f.0.clone(), f.1, f.2))
            .collect();
        base.sort_by_key(|f| std::cmp::Reverse(f.1.size()));
        base
    };
    let extensions =
        |fields: &[(String, FieldKind, Option<u8>, bool)]| -> Vec<(String, FieldKind, Option<u8>)> {
            fields
                .iter()
                .filter(|f| f.3)
                .map(|f| (f.0.clone(), f.1, f.2))
                .collect()
        };
    if message.name != built_in.name() || base(&ours) != base(&theirs) {
        return Some(MessageChange::Layout);
    }
    let (ours, theirs) = (extensions(&ours), extensions(&theirs));
    // An older MAVLink's message, without extensions added since, decodes
    // the same with UXDuck's own.
    (!theirs.starts_with(&ours)).then_some(MessageChange::Extensions)
}

const fn field_kind(kind: Kind) -> FieldKind {
    match kind {
        Kind::U8 => FieldKind::U8,
        Kind::I8 => FieldKind::I8,
        Kind::U16 => FieldKind::U16,
        Kind::I16 => FieldKind::I16,
        Kind::U32 => FieldKind::U32,
        Kind::I32 => FieldKind::I32,
        Kind::U64 => FieldKind::U64,
        Kind::I64 => FieldKind::I64,
        Kind::F32 => FieldKind::F32,
        Kind::F64 => FieldKind::F64,
        Kind::Char => FieldKind::Char,
    }
}

const fn kind(kind: FieldKind) -> Kind {
    match kind {
        FieldKind::U8 => Kind::U8,
        FieldKind::I8 => Kind::I8,
        FieldKind::U16 => Kind::U16,
        FieldKind::I16 => Kind::I16,
        FieldKind::U32 => Kind::U32,
        FieldKind::I32 => Kind::I32,
        FieldKind::U64 => Kind::U64,
        FieldKind::I64 => Kind::I64,
        FieldKind::F32 => Kind::F32,
        FieldKind::F64 => Kind::F64,
        FieldKind::Char => Kind::Char,
    }
}

/// The fields in the order a payload holds them: those before
/// `<extensions/>` largest type first (stable), then the extensions as
/// declared.
fn wire(fields: &[CustomField]) -> Vec<&CustomField> {
    let mut base: Vec<&CustomField> = fields.iter().filter(|f| !f.extension).collect();
    base.sort_by_key(|f| std::cmp::Reverse(f.kind.size()));
    base.extend(fields.iter().filter(|f| f.extension));
    base
}

/// MAVLink's checksum, X.25, over `bytes` from `crc`.
fn x25(mut crc: u16, bytes: &[u8]) -> u16 {
    for &byte in bytes {
        let mut tmp = byte ^ crc.to_le_bytes()[0];
        tmp ^= tmp << 4;
        let tmp = u16::from(tmp);
        crc = (crc >> 8) ^ (tmp << 8) ^ (tmp << 3) ^ (tmp >> 4);
    }
    crc
}

/// A message's `CRC_EXTRA`: the checksum of its name and of the type, name
/// and length of each field before `<extensions/>` in wire order, folded
/// to a byte, as MAVLink's generators compute it.
#[must_use]
pub fn crc_extra(name: &str, fields: &[CustomField]) -> u8 {
    let mut crc = x25(0xFFFF, format!("{name} ").as_bytes());
    for field in wire(fields).into_iter().filter(|f| !f.extension) {
        crc = x25(crc, format!("{} ", field.kind.name()).as_bytes());
        crc = x25(crc, format!("{} ", field.name).as_bytes());
        if let Some(length) = field.length {
            crc = x25(crc, &[length]);
        }
    }
    let [low, high] = crc.to_le_bytes();
    low ^ high
}

/// One decoded message.
#[derive(Clone, Debug, PartialEq)]
pub struct Decoded {
    pub name: String,
    /// By MAVLink's names, raw values in MAVLink's units, as
    /// [`crate::dialect::decode`] gives them.
    pub fields: Map<String, Value>,
    /// The enum entry, or bits, naming a field's value, by field name.
    pub labels: Map<String, Value>,
    /// The units its dialect gives a field, by field name.
    pub units: Map<String, Value>,
    /// The label of the dialect it came from.
    pub dialect: String,
    pub change: MessageChange,
}

/// Decodes a payload by one definition: MAVLink 2's trimmed zeros put back.
#[must_use]
pub fn decode_with(message: &CustomMessage, payload: &[u8]) -> Map<String, Value> {
    let size: usize = message.fields.iter().map(CustomField::size).sum();
    let mut bytes = payload.to_vec();
    bytes.resize(size.max(payload.len()), 0);
    let mut at = 0;
    let mut fields = Map::new();
    for field in wire(&message.fields) {
        let end = at + field.size();
        fields.insert(
            field.name.clone(),
            crate::dialect::read_value(
                kind(field.kind),
                field.length.map(usize::from),
                &bytes[at..end],
            ),
        );
        at = end;
    }
    fields
}

/// The messages and enums that describe one vehicle: its dialects', the
/// newest dialect winning an id, a name or an enum.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Messages {
    by_id: HashMap<u32, (CustomMessage, String)>,
    enums: HashMap<String, CustomEnum>,
}

impl Messages {
    /// From a vehicle's dialects, oldest first.
    #[must_use]
    pub fn of<'a>(dialects: impl IntoIterator<Item = &'a Dialect>) -> Self {
        let mut out = Self::default();
        for dialect in dialects {
            for message in &dialect.messages {
                out.by_id.retain(|_, (m, _)| m.name != message.name);
                out.by_id
                    .insert(message.id, (message.clone(), dialect.label.clone()));
            }
            for found in &dialect.enums {
                out.enums.insert(found.name.clone(), found.clone());
            }
        }
        out
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    #[must_use]
    pub fn get(&self, id: u32) -> Option<&CustomMessage> {
        self.by_id.get(&id).map(|(m, _)| m)
    }

    /// Decodes a payload with this vehicle's definition of `id`, None
    /// when its dialects do not have one.
    #[must_use]
    pub fn decode(&self, id: u32, payload: &[u8]) -> Option<Decoded> {
        let (message, label) = self.by_id.get(&id)?;
        let fields = decode_with(message, payload);
        let units = message
            .fields
            .iter()
            .filter_map(|f| Some((f.name.clone(), Value::String(f.units.clone()?))))
            .collect();
        let mut labels = Map::new();
        for field in &message.fields {
            let (Some(name), None) = (&field.enum_name, field.length) else {
                continue;
            };
            let Some(found) = self.enums.get(name) else {
                continue;
            };
            let Some(value) = fields.get(&field.name).and_then(Value::as_u64) else {
                continue;
            };
            if let Some(text) = found.label(value) {
                labels.insert(field.name.clone(), Value::String(text));
            }
        }
        Some(Decoded {
            name: message.name.clone(),
            fields,
            labels,
            units,
            dialect: label.clone(),
            change: message.change,
        })
    }
}

static BOUND: LazyLock<RwLock<HashMap<String, Arc<Messages>>>> = LazyLock::new(RwLock::default);
/// Each dialect's message ids and their checksums' seeds, by dialect id.
type Checksums = HashMap<String, Vec<(u32, u8)>>;
static CHECKSUMS: LazyLock<RwLock<Checksums>> = LazyLock::new(RwLock::default);

/// The engine says which messages describe a vehicle; empty ones unbind it.
pub fn publish(vehicle_id: &str, messages: Messages) {
    let mut bound = BOUND.write().unwrap_or_else(PoisonError::into_inner);
    if messages.is_empty() {
        bound.remove(vehicle_id);
    } else {
        bound.insert(vehicle_id.to_owned(), Arc::new(messages));
    }
}

/// The messages describing the first of these vehicle ids that has any.
#[must_use]
pub fn for_vehicle(vehicle_ids: &[String]) -> Option<Arc<Messages>> {
    let bound = BOUND.read().unwrap_or_else(PoisonError::into_inner);
    vehicle_ids.iter().find_map(|id| bound.get(id).cloned())
}

/// The engine says which checksums a dialect brings, by its id, whatever it
/// is bound to: a vehicle's commit is not known before its first frames.
pub fn publish_checksums(dialect: &Dialect) {
    let mut sums = CHECKSUMS.write().unwrap_or_else(PoisonError::into_inner);
    sums.insert(
        dialect.id.clone(),
        dialect
            .messages
            .iter()
            .map(|m| (m.id, m.crc_extra))
            .collect(),
    );
}

pub fn forget_checksums(dialect_id: &str) {
    CHECKSUMS
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(dialect_id);
}

/// Whether whole-frame bytes hold a checksum that some added dialect's
/// definition of its id gives.
#[must_use]
pub fn checksum_matches(bytes: &[u8]) -> bool {
    let (header, id) = match bytes.first() {
        Some(0xFE) if bytes.len() >= 8 => (6, u32::from(bytes[5])),
        Some(0xFD) if bytes.len() >= 12 => {
            (10, u32::from_le_bytes([bytes[7], bytes[8], bytes[9], 0]))
        }
        _ => return false,
    };
    let end = header + usize::from(bytes[1]);
    let Some(sent) = bytes.get(end..end + 2) else {
        return false;
    };
    let sent = u16::from_le_bytes([sent[0], sent[1]]);
    let sums = CHECKSUMS.read().unwrap_or_else(PoisonError::into_inner);
    if sums.is_empty() {
        return false;
    }
    let body = x25(0xFFFF, &bytes[1..end]);
    sums.values()
        .flatten()
        .any(|(of, extra)| *of == id && x25(body, &[*extra]) == sent)
}

/// A MAVLink 2 frame from system 1, component 1, of a payload up to 255
/// bytes, checked with `extra`.
pub(crate) fn frame(id: u32, payload: &[u8], extra: u8) -> Vec<u8> {
    frame_from((1, 1, 0), id, payload, extra)
}

/// A MAVLink 2 frame from a system and component, with a sequence number,
/// its payload's trailing zeros dropped as MAVLink 2 sends it.
pub(crate) fn frame_from(
    (system_id, component_id, sequence): (u8, u8, u8),
    id: u32,
    payload: &[u8],
    extra: u8,
) -> Vec<u8> {
    let [a, b, c, _] = id.to_le_bytes();
    let sent = payload
        .iter()
        .rposition(|b| *b != 0)
        .map_or(1, |last| last + 1);
    let length = u8::try_from(sent.min(payload.len())).unwrap_or(u8::MAX);
    let mut frame = vec![
        0xFD,
        length,
        0,
        0,
        sequence,
        system_id,
        component_id,
        a,
        b,
        c,
    ];
    frame.extend_from_slice(&payload[..usize::from(length)]);
    let crc = x25(x25(0xFFFF, &frame[1..]), &[extra]);
    frame.extend_from_slice(&crc.to_le_bytes());
    frame
}

/// One file's messages and enums, not yet compared.
struct Parsed {
    messages: Vec<CustomMessage>,
    enums: Vec<CustomEnum>,
    commands: Vec<CustomCommand>,
}

/// Longest description kept, in characters.
const DESCRIPTION: usize = 400;

fn description(text: &str) -> Option<String> {
    let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!words.is_empty()).then(|| words.chars().take(DESCRIPTION).collect())
}

fn attribute(tag: &quick_xml::events::BytesStart, key: &str) -> Option<String> {
    tag.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == key)
        .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok())
        .map(|v| v.trim().to_owned())
}

/// An enum entry's value: decimal, `0x` hexadecimal or `2**n`.
fn entry_value(text: &str) -> Option<u64> {
    let text = text.trim();
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return u64::from_str_radix(hex, 16).ok();
    }
    if let Some(power) = text.strip_prefix("2**") {
        return power.parse::<u32>().ok().and_then(|n| 1_u64.checked_shl(n));
    }
    text.parse().ok()
}

fn valid_name(name: &str, upper: bool) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| {
            b.is_ascii_digit()
                || b == b'_'
                || if upper {
                    b.is_ascii_uppercase()
                } else {
                    b.is_ascii_alphabetic()
                }
        })
}

/// A command's parameter, closed with its text as its description: kept
/// unless reserved or given twice.
fn end_param(
    param: &mut Option<(CommandParam, bool)>,
    command: Option<&mut CustomCommand>,
    text: &str,
) {
    let (Some((mut done, reserved)), Some(command)) = (param.take(), command) else {
        return;
    };
    if reserved || command.params.iter().any(|p| p.index == done.index) {
        return;
    }
    done.description = description(text);
    command.params.push(done);
    command.params.sort_by_key(|p| p.index);
}

#[allow(clippy::too_many_lines, reason = "one arm per element")]
fn parse(xml: &str) -> Result<Parsed, String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut out = Parsed {
        messages: Vec::new(),
        enums: Vec::new(),
        commands: Vec::new(),
    };
    // Inside `<enum name="MAV_CMD">`: its entries are commands too.
    let mut in_commands = false;
    let mut command: Option<CustomCommand> = None;
    let mut param: Option<(CommandParam, bool)> = None;
    let mut message: Option<CustomMessage> = None;
    let mut extension = false;
    let mut found: Option<CustomEnum> = None;
    let mut text = String::new();
    let mut depth_entry = false;
    loop {
        let event = reader
            .read_event()
            .map_err(|e| format!("not MAVLink XML: {e}"))?;
        match event {
            Event::Start(ref tag) | Event::Empty(ref tag) => {
                let empty = matches!(event, Event::Empty(_));
                text.clear();
                match tag.name().as_ref() {
                    "message" => {
                        let name = attribute(tag, "name").unwrap_or_default();
                        let id = attribute(tag, "id")
                            .and_then(|id| id.parse::<u32>().ok())
                            .filter(|id| *id < 1 << 24)
                            .ok_or_else(|| {
                                format!("message {name:?} has no id from 0 to 16777215")
                            })?;
                        if !valid_name(&name, true) {
                            return Err(format!(
                                "message {id}'s name {name:?} must be capitals, digits and underscores"
                            ));
                        }
                        extension = false;
                        message = Some(CustomMessage {
                            id,
                            name,
                            change: MessageChange::New,
                            crc_extra: 0,
                            description: None,
                            fields: Vec::new(),
                        });
                    }
                    "extensions" => extension = true,
                    "field" => {
                        let Some(open) = message.as_mut() else {
                            continue;
                        };
                        let name = attribute(tag, "name").unwrap_or_default();
                        if !valid_name(&name, false) {
                            return Err(format!(
                                "{}: a field's name {name:?} is not valid",
                                open.name
                            ));
                        }
                        let written = attribute(tag, "type").unwrap_or_default();
                        let (base, length) = match written.split_once('[') {
                            Some((base, rest)) => {
                                let length = rest
                                    .strip_suffix(']')
                                    .and_then(|n| n.trim().parse::<u8>().ok())
                                    .filter(|n| *n > 0)
                                    .ok_or_else(|| {
                                        format!("{}.{name}: no MAVLink type {written:?}", open.name)
                                    })?;
                                (base.trim(), Some(length))
                            }
                            None => (written.as_str(), None),
                        };
                        let kind = FieldKind::parse(base).ok_or_else(|| {
                            format!("{}.{name}: no MAVLink type {written:?}", open.name)
                        })?;
                        if open.fields.iter().any(|f| f.name == name) {
                            return Err(format!("{}.{name} is given twice", open.name));
                        }
                        open.fields.push(CustomField {
                            name,
                            kind,
                            length,
                            extension,
                            units: attribute(tag, "units").filter(|u| !u.is_empty()),
                            enum_name: attribute(tag, "enum").filter(|e| !e.is_empty()),
                            description: None,
                        });
                    }
                    "enum" if message.is_none() => {
                        let name = attribute(tag, "name").unwrap_or_default();
                        if !valid_name(&name, false) {
                            return Err(format!("an enum's name {name:?} is not valid"));
                        }
                        let bitmask = attribute(tag, "bitmask").is_some_and(|b| b == "true");
                        in_commands = !empty && name == "MAV_CMD";
                        let opened = CustomEnum {
                            name,
                            bitmask,
                            entries: Vec::new(),
                        };
                        if empty {
                            out.enums.push(opened);
                        } else {
                            found = Some(opened);
                        }
                    }
                    "entry" => {
                        depth_entry = !empty;
                        let Some(open) = found.as_mut() else {
                            continue;
                        };
                        let name = attribute(tag, "name").unwrap_or_default();
                        let value = attribute(tag, "value").and_then(|v| entry_value(&v));
                        let (true, Some(value)) = (valid_name(&name, false), value) else {
                            continue;
                        };
                        if in_commands && let Ok(number) = u16::try_from(value) {
                            let opened = CustomCommand {
                                value: number,
                                name: name.clone(),
                                description: None,
                                has_location: attribute(tag, "hasLocation")
                                    .is_some_and(|v| v == "true"),
                                no_location: attribute(tag, "hasLocation")
                                    .is_some_and(|v| v == "false"),
                                params: Vec::new(),
                            };
                            if empty {
                                out.commands.push(opened);
                            } else {
                                command = Some(opened);
                            }
                        }
                        open.entries.retain(|e| e.value != value);
                        open.entries.push(EnumEntry { value, name });
                    }
                    "param" => {
                        if command.is_none() {
                            continue;
                        }
                        let Some(index) = attribute(tag, "index")
                            .and_then(|i| i.parse::<u8>().ok())
                            .filter(|i| (1..=7).contains(i))
                        else {
                            continue;
                        };
                        let opened = CommandParam {
                            index,
                            label: attribute(tag, "label").filter(|l| !l.is_empty()),
                            units: attribute(tag, "units").filter(|u| !u.is_empty()),
                            description: None,
                        };
                        let reserved = attribute(tag, "reserved").is_some_and(|r| r == "true");
                        param = Some((opened, reserved));
                        if empty {
                            end_param(&mut param, command.as_mut(), "");
                        }
                    }
                    _ => {}
                }
            }
            Event::Text(t) => text.push_str(&t.xml10_content()),
            Event::CData(t) => text.push_str(&t.xml10_content()),
            Event::GeneralRef(raw) => {
                let reference = format!("&{};", raw.as_ref());
                text.push_str(
                    &quick_xml::escape::unescape(&reference)
                        .map_err(|e| format!("not MAVLink XML: {e}"))?,
                );
            }
            Event::End(tag) => match tag.name().as_ref() {
                "description" if !depth_entry => {
                    if let Some(open) = message.as_mut()
                        && open.description.is_none()
                        && open.fields.is_empty()
                    {
                        open.description = description(&text);
                    }
                }
                "description" => {
                    if param.is_none()
                        && let Some(open) = command.as_mut()
                        && open.description.is_none()
                    {
                        open.description = description(&text);
                    }
                }
                "param" => end_param(&mut param, command.as_mut(), &text),
                "entry" => {
                    depth_entry = false;
                    if let Some(done) = command.take() {
                        out.commands.push(done);
                    }
                }
                "field" => {
                    if let Some(last) = message.as_mut().and_then(|m| m.fields.last_mut()) {
                        last.description = description(&text);
                    }
                }
                "message" => {
                    if let Some(mut done) = message.take() {
                        if done.fields.is_empty() || done.fields.len() > 64 {
                            return Err(format!("{}: a message has 1 to 64 fields", done.name));
                        }
                        let size: usize = done.fields.iter().map(CustomField::size).sum();
                        if size > 255 {
                            return Err(format!(
                                "{}: {size} bytes, more than a frame's 255",
                                done.name
                            ));
                        }
                        done.crc_extra = crc_extra(&done.name, &done.fields);
                        out.messages.push(done);
                    }
                }
                "enum" => {
                    in_commands = false;
                    if let Some(done) = found.take() {
                        out.enums.push(done);
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

    use super::*;

    const ACME: &str = include_str!("../fixtures/acme_fork.xml");

    #[test]
    fn crc_extra_of_every_built_in_message_is_mavios() {
        use mavio::dialects::ardupilotmega::Ardupilotmega;
        use mavio::protocol::Dialect as _;
        for message in crate::dialect::messages() {
            let fields: Vec<CustomField> = message
                .fields()
                .map(|f| CustomField {
                    name: f.name.to_owned(),
                    kind: field_kind(f.kind),
                    length: f.length.and_then(|n| u8::try_from(n).ok()),
                    extension: f.extension,
                    units: None,
                    enum_name: None,
                    description: None,
                })
                .collect();
            let theirs = Ardupilotmega::message_info(message.id())
                .unwrap()
                .crc_extra();
            assert_eq!(
                crc_extra(message.name(), &fields),
                theirs,
                "{}",
                message.name()
            );
        }
    }

    /// Every built-in message, as a person's definition, decodes random
    /// payloads of every length as the built-in table does.
    #[test]
    fn built_in_messages_decode_the_same_as_definitions() {
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut random = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        };
        for built_in in crate::dialect::messages() {
            let fields: Vec<CustomField> = built_in
                .fields()
                .map(|f| CustomField {
                    name: f.name.to_owned(),
                    kind: field_kind(f.kind),
                    length: f.length.and_then(|n| u8::try_from(n).ok()),
                    extension: f.extension,
                    units: None,
                    enum_name: None,
                    description: None,
                })
                .collect();
            let size: usize = fields.iter().map(CustomField::size).sum();
            let message = CustomMessage {
                id: built_in.id(),
                name: built_in.name().to_owned(),
                change: MessageChange::New,
                crc_extra: 0,
                description: None,
                fields,
            };
            for length in [0, size / 2, size] {
                let payload: Vec<u8> = (0..length).map(|_| random()).collect();
                let theirs = crate::dialect::decode(built_in.id(), &payload).unwrap();
                let ours = decode_with(&message, &payload);
                let pairs = |m: &Map<String, Value>| {
                    m.iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect::<Vec<_>>()
                };
                assert_eq!(pairs(&ours), pairs(&theirs.fields), "{}", built_in.name());
            }
        }
    }

    #[test]
    fn a_forks_dialect_keeps_what_is_new_or_changed() {
        use mavio::dialects::ardupilotmega::Ardupilotmega;
        use mavio::protocol::Dialect as _;
        let read = read(&[("acme.xml", ACME)]).unwrap();
        let by_name: HashMap<&str, &CustomMessage> =
            read.messages.iter().map(|m| (m.name.as_str(), m)).collect();
        assert_eq!(by_name["ACME_STATUS"].change, MessageChange::New);
        assert_eq!(by_name["SYS_STATUS"].change, MessageChange::Extensions);
        assert_eq!(by_name["GLOBAL_POSITION_INT"].change, MessageChange::Layout);
        assert!(
            !by_name.contains_key("HEARTBEAT"),
            "the same as the built-in one"
        );
        assert_eq!(read.unchanged, 1);
        assert_eq!(read.messages.len(), 3);
        // ACME_MODE is named by ACME_STATUS; ACME_UNUSED by nothing kept.
        let enums: Vec<&str> = read.enums.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(enums, ["ACME_FLAGS", "ACME_MODE"]);
        let status = by_name["ACME_STATUS"];
        assert_eq!(status.description.as_deref(), Some("Acme payload status."));
        let mode = status.fields.iter().find(|f| f.name == "mode").unwrap();
        assert_eq!(mode.enum_name.as_deref(), Some("ACME_MODE"));
        assert_eq!(
            mode.description.as_deref(),
            Some("What the payload is doing.")
        );
        // Extensions keep the built-in checksum seed.
        let sys = by_name["SYS_STATUS"];
        assert_eq!(
            sys.crc_extra,
            Ardupilotmega::message_info(1).unwrap().crc_extra()
        );
        assert_ne!(
            by_name["GLOBAL_POSITION_INT"].crc_extra,
            Ardupilotmega::message_info(33).unwrap().crc_extra()
        );
    }

    #[test]
    fn an_older_mavlinks_message_is_not_a_change() {
        let fields: String = ACME
            .lines()
            .skip_while(|l| !l.contains("name=\"SYS_STATUS\""))
            .take_while(|l| !l.contains("<extensions/>"))
            .collect();
        let older = format!("<mavlink><messages>{fields}</message></messages></mavlink>");
        assert_eq!(
            read(&[("older.xml", &older)]).err().as_deref(),
            Some("all 1 messages are the same as UXDuck's ArduPilot dialect: nothing to add")
        );
    }

    #[test]
    fn refusals_say_why() {
        let same = "<mavlink><messages><message id=\"0\" name=\"HEARTBEAT\">\
            <field type=\"uint8_t\" name=\"type\"/><field type=\"uint8_t\" name=\"autopilot\"/>\
            <field type=\"uint8_t\" name=\"base_mode\"/><field type=\"uint32_t\" name=\"custom_mode\"/>\
            <field type=\"uint8_t\" name=\"system_status\"/>\
            <field type=\"uint8_t_mavlink_version\" name=\"mavlink_version\"/></message></messages></mavlink>";
        let error = read(&[("same.xml", same)]).unwrap_err();
        assert!(
            error.contains("same as UXDuck's ArduPilot dialect"),
            "{error}"
        );
        let error = read(&[("a.xml", ACME), ("b.xml", ACME)]).unwrap_err();
        assert!(error.contains("b.xml"), "{error}");
        let error = read(&[(
            "bad.xml",
            "<mavlink><messages><message id=\"x\" name=\"A\"/>",
        )])
        .unwrap_err();
        assert!(error.contains("no id"), "{error}");
        let error = read(&[(
            "big.xml",
            "<mavlink><messages><message id=\"50000\" name=\"BIG\">\
             <field type=\"uint8_t[200]\" name=\"a\"/><field type=\"uint8_t[100]\" name=\"b\"/>\
             </message></messages></mavlink>",
        )])
        .unwrap_err();
        assert!(error.contains("255"), "{error}");
        assert!(read(&[]).is_err());
    }

    #[test]
    fn a_dialects_own_commands_are_kept_with_the_params_they_use() {
        let xml = r#"<mavlink><enums><enum name="MAV_CMD">
            <entry value="16" name="MAV_CMD_NAV_WAYPOINT"><param index="1" label="Hold">Hold.</param></entry>
            <entry value="36" name="MAV_CMD_NAV_ARC_WAYPOINT" hasLocation="true" isDestination="true">
              <description>Go round an arc to the point.</description>
              <param index="1" label="Arc Angle" units="deg">Angle of the arc.</param>
              <param index="2" reserved="true" default="NaN"/>
              <param index="5" label="Latitude">Latitude</param>
              <param index="7" label="Altitude" units="m">Altitude</param>
            </entry>
            <entry value="31000" name="MAV_CMD_ACME_DROP">
              <param index="1" label="Bay" minValue="1" maxValue="4">Which bay opens.</param>
              <param index="2">Seconds it stays open.</param>
              <description>Drop the payload.</description>
            </entry>
            <entry value="31001" name="MAV_CMD_ACME_PING"/>
            <entry value="31002" name="MAV_CMD_WAYPOINT_USER_3"/>
          </enum></enums></mavlink>"#;
        let kept = read(&[("acme_commands.xml", xml)]).unwrap();
        assert!(
            kept.messages.is_empty(),
            "commands alone are something to add"
        );
        let values: Vec<u16> = kept.commands.iter().map(|c| c.value).collect();
        assert_eq!(
            values,
            [36, 31_000, 31_001],
            "16 and 31002 are the built-in dialect's; 31000 and 31001 are named the fork's way"
        );
        let arc = &kept.commands[0];
        assert!(arc.has_location);
        assert_eq!(
            arc.description.as_deref(),
            Some("Go round an arc to the point.")
        );
        let indices: Vec<u8> = arc.params.iter().map(|p| p.index).collect();
        assert_eq!(indices, [1, 5, 7], "the reserved param 2 is left out");
        assert_eq!(arc.params[0].label.as_deref(), Some("Arc Angle"));
        assert_eq!(arc.params[0].units.as_deref(), Some("deg"));
        assert_eq!(
            arc.params[0].description.as_deref(),
            Some("Angle of the arc.")
        );
        let drop = &kept.commands[1];
        assert!(!drop.has_location);
        assert_eq!(drop.description.as_deref(), Some("Drop the payload."));
        assert_eq!(drop.params[1].label, None);
        assert_eq!(
            drop.params[1].description.as_deref(),
            Some("Seconds it stays open.")
        );
        assert_eq!(kept.commands[2].name, "MAV_CMD_ACME_PING");
        assert!(kept.commands[2].params.is_empty());

        let known = r#"<mavlink><enums><enum name="MAV_CMD"><entry value="16" name="MAV_CMD_NAV_WAYPOINT"/></enum></enums></mavlink>"#;
        assert!(read(&[("known.xml", known)]).is_err(), "nothing new to add");
    }

    #[test]
    fn only_a_user_command_or_a_new_number_may_be_without_a_place() {
        let xml = r#"<mavlink><enums><enum name="MAV_CMD">
            <entry value="16" name="MAV_CMD_ACME_HOP" hasLocation="false"/>
            <entry value="31000" name="MAV_CMD_ACME_DROP" hasLocation="false"/>
            <entry value="31010" name="MAV_CMD_ACME_SPRAY" hasLocation="false"/>
            <entry value="31014" name="MAV_CMD_ACME_RINSE" hasLocation="false"/>
            <entry value="39000" name="MAV_CMD_ACME_FOLD" hasLocation="false"/>
          </enum></enums></mavlink>"#;
        let kept = read(&[("acme_commands.xml", xml)]).unwrap();
        let placeless: Vec<(u16, bool)> = kept
            .commands
            .iter()
            .map(|c| (c.value, c.no_location))
            .collect();
        assert_eq!(
            placeless,
            [
                (16, false),
                (31_000, false),
                (31_010, true),
                (31_014, true),
                (39_000, true)
            ],
            "a waypoint renamed, or a waypoint user command, keeps its place"
        );
    }

    /// S338's fixture: a message with units and an enum, and a command
    /// declared without a place, read and decoded as the inspector and a
    /// plan see them.
    #[test]
    fn a_small_dialect_gives_units_enum_names_and_a_command_without_a_place() {
        let xml = include_str!("../fixtures/acme_survey.xml");
        let kept = read(&[("acme_survey.xml", xml)]).unwrap();
        let spray = &kept.commands[0];
        assert_eq!(
            (spray.value, spray.name.as_str()),
            (31_010, "MAV_CMD_ACME_SPRAY")
        );
        assert!(spray.no_location && !spray.has_location);
        let params: Vec<(u8, Option<&str>, Option<&str>)> = spray
            .params
            .iter()
            .map(|p| (p.index, p.label.as_deref(), p.units.as_deref()))
            .collect();
        assert_eq!(
            params,
            [
                (1, Some("Rate"), Some("l/min")),
                (2, Some("Seconds"), Some("s")),
                (3, Some("Nozzle"), None),
                (5, Some("Pressure"), Some("kPa")),
                (7, Some("Height"), Some("m")),
            ],
            "reserved params 4 and 6 are left out"
        );

        let messages = Messages::of([&dialect(kept, "Acme survey")]);
        let mut payload = 1_234_u32.to_le_bytes().to_vec();
        payload.extend_from_slice(&12.5_f32.to_le_bytes());
        payload.extend_from_slice(&900_u16.to_le_bytes());
        payload.push(1);
        let decoded = messages.decode(43_010, &payload).unwrap();
        assert_eq!(decoded.name, "ACME_SPRAY_STATUS");
        assert_eq!(decoded.fields["flow"], 12.5);
        assert_eq!(decoded.fields["tank"], 900);
        assert_eq!(decoded.labels["state"], "ACME_SPRAY_ON");
        assert_eq!(decoded.units["flow"], "l/min");
        assert_eq!(decoded.units["tank"], "ml");
        assert_eq!(decoded.units["time_boot_ms"], "ms");
        assert!(decoded.units.get("state").is_none(), "no units in the file");
        assert!(
            messages.decode(43_011, &payload).is_none(),
            "an id no dialect has is left to show raw"
        );
    }

    fn dialect(read: Read, label: &str) -> Dialect {
        Dialect {
            id: label.to_owned(),
            label: label.to_owned(),
            files: Vec::new(),
            commits: Vec::new(),
            every_vehicle: false,
            messages: read.messages,
            enums: read.enums,
            commands: read.commands,
            unchanged: read.unchanged,
            added_at_us: 0,
            updated_at_us: 0,
            profile_id: None,
            places_read: true,
        }
    }

    #[test]
    fn a_vehicles_messages_decode_with_enum_names() {
        let acme = dialect(read(&[("acme.xml", ACME)]).unwrap(), "Acme");
        let messages = Messages::of([&acme]);
        // ACME_STATUS: time_usec u64, voltage f32, mode u8, flags u8,
        // name char[8], then the extension temperature i16.
        let mut payload = 7_u64.to_le_bytes().to_vec();
        payload.extend_from_slice(&12.5_f32.to_le_bytes());
        payload.extend_from_slice(&[3, 5]);
        payload.extend_from_slice(b"pod\0\0\0\0\0");
        let decoded = messages.decode(43_001, &payload).unwrap();
        assert_eq!(decoded.name, "ACME_STATUS");
        assert_eq!(decoded.dialect, "Acme");
        assert_eq!(decoded.fields["time_usec"], 7);
        assert_eq!(decoded.fields["voltage"], 12.5);
        assert_eq!(decoded.fields["name"], "pod");
        assert_eq!(decoded.fields["temperature"], 0, "a trimmed extension");
        assert_eq!(decoded.labels["mode"], "ACME_MODE_SURVEY");
        assert_eq!(decoded.labels["flags"], "ACME_FLAG_ARMED | ACME_FLAG_HOT");
        assert!(messages.decode(0, &[]).is_none(), "the built-in dialect's");
    }

    #[test]
    fn the_newest_dialect_wins_an_id_or_a_name() {
        let older = dialect(read(&[("acme.xml", ACME)]).unwrap(), "Old");
        let newer_xml = "<mavlink><messages><message id=\"43002\" name=\"ACME_STATUS\">\
            <field type=\"uint16_t\" name=\"level\"/></message></messages></mavlink>";
        let newer = dialect(read(&[("new.xml", newer_xml)]).unwrap(), "New");
        let messages = Messages::of([&older, &newer]);
        assert!(messages.get(43_001).is_none(), "its name went to 43002");
        assert_eq!(messages.decode(43_002, &[1, 0]).unwrap().dialect, "New");
    }

    #[test]
    fn a_changed_messages_checksum_is_accepted_once_published() {
        let mut acme = dialect(read(&[("acme.xml", ACME)]).unwrap(), "Acme");
        acme.id = "checksum-test".into();
        let changed = acme
            .messages
            .iter()
            .find(|m| m.name == "GLOBAL_POSITION_INT")
            .unwrap();
        let payload = vec![1_u8; changed.fields.iter().map(CustomField::size).sum()];
        let frame = mavlink2(33, &payload, changed.crc_extra);
        assert!(!checksum_matches(&frame));
        assert_eq!(crate::decode::check(&frame), crate::framing::Check::Invalid);
        publish_checksums(&acme);
        assert!(checksum_matches(&frame));
        assert_eq!(crate::decode::check(&frame), crate::framing::Check::Valid);
        forget_checksums("checksum-test");
        assert!(!checksum_matches(&frame));
    }

    fn mavlink2(id: u32, payload: &[u8], extra: u8) -> Vec<u8> {
        frame(id, payload, extra)
    }
}

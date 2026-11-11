//! A fork's own MAVLink messages (ADR-0044): dialect files people add,
//! kept as the messages that are new or differ from UXDuck's built-in
//! ArduPilot dialect, with the enums their fields name. Each vehicle uses the
//! dialects matching its firmware's commit, or those a person chose for it
//! (F1 to F3, F13). Small enough to journal and sync.

use serde::{Deserialize, Serialize};

/// The most messages one dialect keeps.
pub const MAX_MESSAGES: usize = 256;
/// The most enums one dialect keeps.
pub const MAX_ENUMS: usize = 64;
/// The most entries one enum keeps.
pub const MAX_ENTRIES: usize = 1024;
/// The most commands one dialect keeps.
pub const MAX_COMMANDS: usize = 256;

/// A dialect a person added.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Dialect {
    pub id: String,
    pub label: String,
    /// The names of the files it was read from, in the order given.
    pub files: Vec<String>,
    /// Vehicles built from any of these commits (7 to 40 hex characters)
    /// use it unless a person chose otherwise for them (F13).
    #[serde(
        default,
        alias = "commit",
        deserialize_with = "one_or_many",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub commits: Vec<String>,
    /// Every vehicle uses it, whatever was chosen; then it only adds
    /// messages (F3).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub every_vehicle: bool,
    /// New and changed messages, by id.
    pub messages: Vec<CustomMessage>,
    /// Enums the messages' fields name, by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enums: Vec<CustomEnum>,
    /// `MAV_CMD` entries the built-in dialect does not have, by number:
    /// the commands a plan using it offers (F14).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<CustomCommand>,
    /// Messages the files had that are the same as the built-in dialect's,
    /// so not kept.
    #[serde(default)]
    pub unchanged: u32,
    pub added_at_us: u64,
    /// When it was added or bound again: its version for sync.
    #[serde(default)]
    pub updated_at_us: u64,
    /// The firmware profile it came with (S254): removed with it, not alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    /// Read by a UXDuck that keeps a command's `hasLocation="false"` as
    /// `no_location` (0.14.0 on). A dialect stored before has no such word:
    /// its commands without a place can't be told from those its file said
    /// nothing about, so their frame stays checked until it is read again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub places_read: bool,
}

/// A commit as one string (S252's journal) or several.
fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Commits {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Option::<Commits>::deserialize(d)? {
        None => Vec::new(),
        Some(Commits::One(one)) => vec![one],
        Some(Commits::Many(many)) => many,
    })
}

/// Which dialects a vehicle uses (ADR-0044 F13), kept with the vehicle.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "type")]
pub enum DialectChoice {
    /// Those whose commits match the vehicle's firmware.
    #[default]
    Automatic,
    /// These, by id, whatever the vehicle runs; none when empty.
    Chosen { ids: Vec<String> },
}

impl DialectChoice {
    #[must_use]
    pub fn is_automatic(&self) -> bool {
        *self == Self::Automatic
    }
}

impl Dialect {
    /// Whether a vehicle built from `commit` (the first 8 characters of a
    /// git hash, as `AUTOPILOT_VERSION` gives it) matches one of its commits.
    #[must_use]
    pub fn matches_commit(&self, commit: Option<&str>) -> bool {
        commit.is_some_and(|theirs| {
            !theirs.is_empty()
                && self
                    .commits
                    .iter()
                    .any(|ours| ours.starts_with(theirs) || theirs.starts_with(ours.as_str()))
        })
    }

    /// Whether a vehicle built from `commit`, with `choice`, uses it.
    #[must_use]
    pub fn describes(&self, choice: &DialectChoice, commit: Option<&str>) -> bool {
        self.every_vehicle
            || match choice {
                DialectChoice::Automatic => self.matches_commit(commit),
                DialectChoice::Chosen { ids } => ids.contains(&self.id),
            }
    }

    /// Whether it changes a message the built-in dialect has.
    #[must_use]
    pub fn changes_standard(&self) -> bool {
        self.messages.iter().any(|m| m.change != MessageChange::New)
    }
}

/// How a kept message differs from the built-in dialect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MessageChange {
    /// An id the built-in dialect does not have.
    New,
    /// Its name, or the fields before `<extensions/>`, differ: its
    /// checksum's `CRC_EXTRA` is not the built-in one, so UXDuck's own
    /// readers do not see it (F5).
    Layout,
    /// Only the fields after `<extensions/>` differ.
    Extensions,
}

/// One message, as its dialect file declares it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessage {
    pub id: u32,
    pub name: String,
    pub change: MessageChange,
    /// The checksum's seed for this definition.
    pub crc_extra: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// In declared order; extensions last.
    pub fields: Vec<CustomField>,
}

/// One field of a message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomField {
    pub name: String,
    pub kind: FieldKind,
    /// Items in an array; none for one value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<u8>,
    /// Declared after `<extensions/>`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub extension: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units: Option<String>,
    /// The enum its values come from.
    #[serde(default, rename = "enum", skip_serializing_if = "Option::is_none")]
    pub enum_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A field's type, or its items' for an array, as MAVLink names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FieldKind {
    #[serde(rename = "uint8_t")]
    U8,
    #[serde(rename = "int8_t")]
    I8,
    #[serde(rename = "uint16_t")]
    U16,
    #[serde(rename = "int16_t")]
    I16,
    #[serde(rename = "uint32_t")]
    U32,
    #[serde(rename = "int32_t")]
    I32,
    #[serde(rename = "uint64_t")]
    U64,
    #[serde(rename = "int64_t")]
    I64,
    #[serde(rename = "float")]
    F32,
    #[serde(rename = "double")]
    F64,
    #[serde(rename = "char")]
    Char,
}

impl FieldKind {
    /// From MAVLink's name; `uint8_t_mavlink_version` is a byte.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "uint8_t" | "uint8_t_mavlink_version" => Self::U8,
            "int8_t" => Self::I8,
            "uint16_t" => Self::U16,
            "int16_t" => Self::I16,
            "uint32_t" => Self::U32,
            "int32_t" => Self::I32,
            "uint64_t" => Self::U64,
            "int64_t" => Self::I64,
            "float" => Self::F32,
            "double" => Self::F64,
            "char" => Self::Char,
            _ => return None,
        })
    }

    /// MAVLink's name, as the checksum's seed spells it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::U8 => "uint8_t",
            Self::I8 => "int8_t",
            Self::U16 => "uint16_t",
            Self::I16 => "int16_t",
            Self::U32 => "uint32_t",
            Self::I32 => "int32_t",
            Self::U64 => "uint64_t",
            Self::I64 => "int64_t",
            Self::F32 => "float",
            Self::F64 => "double",
            Self::Char => "char",
        }
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

impl CustomField {
    /// Bytes in a payload.
    #[must_use]
    pub fn size(&self) -> usize {
        self.kind.size() * self.length.map_or(1, usize::from)
    }
}

/// A `MAV_CMD` entry a dialect adds: a command for missions and
/// `COMMAND_LONG`, with the parameters it uses.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomCommand {
    pub value: u16,
    /// Such as `MAV_CMD_ACME_DROP`.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Its `hasLocation`: params 5 to 7 are a place.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub has_location: bool,
    /// Its `hasLocation="false"`, said in the file: it never has a place,
    /// so the frame a vehicle gives it back in means nothing. A file
    /// without the attribute says neither.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_location: bool,
    /// The parameters it uses, by index; those marked reserved are left out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<CommandParam>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandParam {
    /// 1 to 7.
    pub index: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// An enum a dialect defines, as far as its fields need it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomEnum {
    pub name: String,
    /// Its entries are bits, set together.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bitmask: bool,
    pub entries: Vec<EnumEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnumEntry {
    pub value: u64,
    pub name: String,
}

impl CustomEnum {
    /// A value's name: its entry's, or for a bitmask the names of the
    /// bits set, joined by ` | `. None when nothing names it.
    #[must_use]
    pub fn label(&self, value: u64) -> Option<String> {
        if !self.bitmask {
            return self
                .entries
                .iter()
                .find(|e| e.value == value)
                .map(|e| e.name.clone());
        }
        if value == 0 {
            return self
                .entries
                .iter()
                .find(|e| e.value == 0)
                .map(|e| e.name.clone());
        }
        let names: Vec<&str> = self
            .entries
            .iter()
            .filter(|e| e.value != 0 && value & e.value == e.value)
            .map(|e| e.name.as_str())
            .collect();
        (!names.is_empty()).then(|| names.join(" | "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dialect(commits: &[&str], every_vehicle: bool) -> Dialect {
        Dialect {
            id: "d".into(),
            label: "Acme".into(),
            files: vec!["acme.xml".into()],
            commits: commits.iter().map(|c| (*c).to_owned()).collect(),
            every_vehicle,
            messages: Vec::new(),
            enums: Vec::new(),
            commands: Vec::new(),
            unchanged: 0,
            added_at_us: 0,
            updated_at_us: 0,
            profile_id: None,
            places_read: true,
        }
    }

    #[test]
    fn used_by_commit_by_choice_or_by_every_vehicle() {
        let auto = DialectChoice::Automatic;
        let built = dialect(&["abcdef1234", "1234567"], false);
        assert!(built.describes(&auto, Some("abcdef12")));
        assert!(built.describes(&auto, Some("12345678")));
        assert!(!built.describes(&auto, Some("99999999")));
        assert!(!built.describes(&auto, None));
        let chosen = DialectChoice::Chosen {
            ids: vec!["d".into()],
        };
        let none = DialectChoice::Chosen { ids: Vec::new() };
        assert!(dialect(&[], false).describes(&chosen, None));
        assert!(!built.describes(&none, Some("abcdef12")));
        assert!(!dialect(&[], false).describes(&auto, Some("abcdef12")));
        assert!(dialect(&[], true).describes(&none, None));
    }

    #[test]
    fn one_commit_from_before_reads_as_a_list() {
        let old = r#"{"id":"d","label":"Acme","files":[],"commit":"abcdef12","messages":[],"addedAtUs":0}"#;
        let read: Dialect = serde_json::from_str(old).unwrap();
        assert_eq!(read.commits, vec!["abcdef12".to_owned()]);
        assert!(!read.every_vehicle);
    }

    #[test]
    fn enum_and_bitmask_labels() {
        let modes = CustomEnum {
            name: "ACME_MODE".into(),
            bitmask: false,
            entries: vec![
                EnumEntry {
                    value: 0,
                    name: "ACME_MODE_IDLE".into(),
                },
                EnumEntry {
                    value: 3,
                    name: "ACME_MODE_SURVEY".into(),
                },
            ],
        };
        assert_eq!(modes.label(3).as_deref(), Some("ACME_MODE_SURVEY"));
        assert_eq!(modes.label(2), None);
        let flags = CustomEnum {
            name: "ACME_FLAGS".into(),
            bitmask: true,
            entries: vec![
                EnumEntry {
                    value: 1,
                    name: "ACME_FLAG_A".into(),
                },
                EnumEntry {
                    value: 4,
                    name: "ACME_FLAG_C".into(),
                },
            ],
        };
        assert_eq!(flags.label(5).as_deref(), Some("ACME_FLAG_A | ACME_FLAG_C"));
        assert_eq!(flags.label(2), None);
    }
}

use std::collections::{HashMap, HashSet};

use crc_any::CRCu64;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::protocol::commands::CMD_ENUM_NAME;
use crate::protocol::metadata::DialectMetadata;
use crate::protocol::microservices::Microservices;
use crate::protocol::{
    Command, DialectId, DialectVersion, Enum, EnumEntry, Filter, Fingerprint, Message, MessageId,
};
use crate::utils::{dialect_canonical_name, Buildable, Builder, Named};

/// MAVLink dialect specification.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[cfg_attr(feature = "specta", specta(rename = "MavInspectDialect"))]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Dialect {
    name: String,
    canonical_name: String,
    version: Option<DialectVersion>,
    dialect: Option<DialectId>,
    messages: HashMap<String, Message>,
    enums: HashMap<String, Enum>,
    commands: HashMap<String, Command>,
    includes: HashSet<String>,
    metadata: DialectMetadata,
}

impl Dialect {
    /// Default constructor
    ///
    /// # Arguments
    ///
    /// * `name` dialect name.
    /// * `version` dialect version (if provided).
    /// * `dialect` dialect ID.
    /// * `messages` dialect messages.
    /// * `enums` dialect enums.
    pub fn new(
        name: impl AsRef<str>,
        version: Option<DialectVersion>,
        dialect: Option<DialectId>,
        messages: Vec<Message>,
        enums: Vec<Enum>,
        includes: Vec<String>,
        metadata: DialectMetadata,
    ) -> Self {
        let name = name.as_ref().to_string();
        let canonical_name = dialect_canonical_name(name.as_str());

        let enums = {
            let mut enums_: HashMap<String, Enum> = HashMap::default();
            for enm in enums {
                enums_.insert(enm.name().to_string(), enm);
            }
            enums_
        };
        let messages = {
            let mut messages_: HashMap<String, Message> = HashMap::default();
            for msg in messages {
                messages_.insert(msg.name().to_string(), msg);
            }
            messages_
        };
        let commands = Self::derive_commands_from_enums(&enums);

        let includes = HashSet::from_iter(includes.iter().map(ToString::to_string));

        Self {
            name,
            canonical_name,
            version,
            dialect,
            messages,
            enums,
            commands,
            includes,
            metadata,
        }
    }

    /// Dialect name.
    ///
    /// See [`Self::canonical_name`] for canonical name used as a key in
    /// [`Protocol::dialects`](crate::protocol::Protocol::dialects).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Canonical dialect name.
    ///
    /// As a dialect [name](Self::name) we use a file base name of its XML definition (without extension). However,
    /// upon XML parsing loading, we convert this name into canonical form
    /// (using [`dialect_canonical_name`]). Canonical names are used during dialect discovery by
    /// [`Inspector`](crate::parser::Inspector) and in
    /// [`InspectorBuilder::set_include`](crate::parser::InspectorBuilder::set_include)/[`InspectorBuilder::set_exclude`](crate::parser::InspectorBuilder::set_exclude).
    /// We do this in order to avoid naming collisions when someone tries to generate source code based on dialect names.
    pub fn canonical_name(&self) -> &str {
        &self.canonical_name
    }

    /// Dialect version.
    pub fn version(&self) -> Option<DialectVersion> {
        self.version
    }

    /// Dialect ID.
    pub fn dialect(&self) -> Option<DialectId> {
        self.dialect
    }

    /// Collection of dialect messages.
    pub fn messages(&self) -> impl IntoIterator<Item = &Message> + Clone {
        self.messages.values()
    }

    /// Collection of dialect enums.
    pub fn enums(&self) -> impl IntoIterator<Item = &Enum> + Clone {
        self.enums.values()
    }

    /// Collection of dialect commands.
    pub fn commands(&self) -> impl IntoIterator<Item = &Command> + Clone {
        self.commands.values()
    }

    /// Collection of dialect dependencies.
    ///
    /// Dialect dependencies are specified by the `<include>` XML tag.
    pub fn includes(&self) -> impl IntoIterator<Item = &str> + Clone {
        self.includes.iter().map(|s| s.as_str())
    }

    /// Check that message with specified `name` is present in dialect.
    pub fn contains_message_with_name(&self, name: &str) -> bool {
        self.messages.contains_key(name)
    }

    /// Check that enum with specified `name` is present in dialect.
    pub fn contains_enum_with_name(&self, name: &str) -> bool {
        self.enums.contains_key(name)
    }

    /// Check that command with specified `name` is present in dialect.
    pub fn contains_command_with_name(&self, name: &str) -> bool {
        self.commands.contains_key(name)
    }

    /// Get message with specified `name`.
    pub fn get_message_by_name(&self, name: &str) -> Option<&Message> {
        self.messages.get(name)
    }

    /// Get enum with specified `name`.
    pub fn get_enum_by_name(&self, name: &str) -> Option<&Enum> {
        self.enums.get(name)
    }

    /// Get command with specified `name`.
    pub fn get_command_by_name(&self, name: &str) -> Option<&Command> {
        self.commands.get(name)
    }

    /// Check that message with specified `id` is present in dialect.
    pub fn contains_message_with_id(&self, id: MessageId) -> bool {
        self.messages.values().any(|message| message.id() == id)
    }

    /// Get message with specified `id`.
    pub fn get_message_by_id(&self, id: MessageId) -> Option<&Message> {
        self.messages.values().find(|&message| message.id() == id)
    }

    /// MAVLink [microservices](https://mavlink.io/en/services/) supported by dialect.
    pub fn microservices(&self) -> Microservices {
        Microservices::from_dialect(self)
    }

    /// Returns dialect metadata.
    ///
    /// Upon reading from a location that contains MAVlink XML definitions, [`Inspector`](crate::Inspector)
    /// looks up for a file `.dialects-metadata.yml`. If it exists, it loads its contents into a
    /// [`DialectMetadata`] and attaches to all top-level dialects loaded from this location
    /// (includes are ignored).
    ///
    /// <section class="warning">
    /// Metadata support is unstable. To access it, use `unstable` feature flag.
    /// </section>
    pub fn metadata(&self) -> &DialectMetadata {
        &self.metadata
    }

    /// Creates a new instance of [`Dialect`] with entities matching [`Filter`].
    ///
    /// Use [`Dialect::retain`] to filter an existing instance instead.
    pub fn filtered(&self, filter: &Filter) -> Self {
        let mut dialect = self.clone();
        dialect.retain(filter);
        dialect
    }

    /// Retain only entities matching provided [`Filter`].
    ///
    /// This is an in-place version on [`Dialect::filtered`]. The latter create a new filtered instance.
    pub fn retain(&mut self, filter: &Filter) {
        if filter.is_none() {
            return;
        }

        self.retain_entities(
            filter.microservices(),
            filter.messages(),
            filter.enums(),
            filter.commands(),
        );
    }

    /// Dialect fingerprint.
    ///
    /// A value of opaque type [`Fingerprint`] that contains dialect CRC.
    pub fn fingerprint(&self) -> Fingerprint {
        let mut crc_calculator = CRCu64::crc64();

        crc_calculator.digest(format!("{:?} ", self.name).as_bytes());

        let mut messages: Vec<&Message> = self.messages.values().collect();
        messages.sort_by_key(|a| a.id());
        for msg in messages {
            crc_calculator.digest(&msg.fingerprint_strict(Some(self)).as_bytes());
        }

        let mut enums: Vec<&Enum> = self.enums.values().collect();
        enums.sort_by_key(|a| a.name().to_string());
        for enm in enums {
            crc_calculator.digest(&enm.fingerprint().as_bytes());
        }

        if let Some(version) = self.version {
            crc_calculator.digest(&version.to_le_bytes());
        }
        if let Some(dialect) = self.dialect {
            crc_calculator.digest(&dialect.to_le_bytes());
        }

        crc_calculator.get_crc().into()
    }

    fn retain_entities(
        &mut self,
        microservices: Option<Microservices>,
        messages: Option<&[impl AsRef<str>]>,
        enums: Option<&[impl AsRef<str>]>,
        commands: Option<&[impl AsRef<str>]>,
    ) {
        let _empty: [String; 0] = [];
        let _microservices = microservices;
        macro_rules! collect_names {
            ($subject: ident) => {
                match (_microservices, $subject) {
                    (None, None) => HashSet::from(["*".to_string()]),
                    (None, Some($subject)) => {
                        self.collect_entity_names(&_empty, $subject, self.$subject())
                    }
                    (Some(microservices), None) => self.collect_entity_names(
                        microservices.$subject(),
                        &_empty,
                        self.$subject(),
                    ),
                    (Some(microservices), Some($subject)) => self.collect_entity_names(
                        microservices.$subject(),
                        $subject,
                        self.$subject(),
                    ),
                }
            };
        }

        let message_names = collect_names!(messages);
        let command_names = collect_names!(commands);
        let enum_names = {
            let mut enum_names = collect_names!(enums);

            self.add_command_enums(&mut enum_names, &command_names);
            self.add_message_enums(&mut enum_names, &message_names);

            enum_names
        };

        self.messages
            .retain(|_, msg| message_names.contains(msg.name()));
        self.enums.retain(|_, enm| enum_names.contains(enm.name()));
        self.retain_mav_cmd_entries(&command_names);

        self.commands = Self::derive_commands_from_enums(&self.enums);
    }

    fn collect_entity_names(
        &self,
        base: impl IntoIterator<Item = impl AsRef<str>>,
        extra: impl IntoIterator<Item = impl AsRef<str>>,
        all: impl IntoIterator<Item = impl Named> + Clone,
    ) -> HashSet<String> {
        let mut entities: HashSet<String> =
            HashSet::from_iter(base.into_iter().map(|s| s.as_ref().to_string()));
        let extra: HashSet<String> =
            HashSet::from_iter(extra.into_iter().map(|s| s.as_ref().to_string()));
        if !extra.is_empty() {
            entities = entities.union(&extra).cloned().collect()
        }

        for entity in all.into_iter() {
            for entity_name in entities.clone().iter() {
                let insert = match entity_name.as_str() {
                    "*" => true,
                    wildcard
                        if wildcard.ends_with('*')
                            && entity.name().starts_with(wildcard.trim_end_matches('*')) =>
                    {
                        true
                    }
                    name if name == entity.name() => true,
                    _ => false,
                };

                if insert {
                    entities.insert(entity.name().to_string());
                }
            }
        }

        entities
    }

    fn add_command_enums(&self, enum_names: &mut HashSet<String>, command_names: &HashSet<String>) {
        if !command_names.is_empty() {
            enum_names.insert(CMD_ENUM_NAME.into());

            for cmd_name in command_names {
                if let Some(command) = self.commands.get(cmd_name.as_str()) {
                    for param in command.params() {
                        if let Some(enum_name) = param.r#enum() {
                            enum_names.insert(enum_name.into());
                        }
                    }
                }
            }
        }
    }

    fn add_message_enums(&self, enum_names: &mut HashSet<String>, message_names: &HashSet<String>) {
        for msg_name in message_names {
            if let Some(message) = self.messages.get(msg_name) {
                for field in message.fields() {
                    if let Some(enum_name) = field.r#enum() {
                        enum_names.insert(enum_name.into());
                    }
                }
            }
        }
    }

    fn derive_commands_from_enums(enums: &HashMap<String, Enum>) -> HashMap<String, Command> {
        let mut commands: HashMap<String, Command> = HashMap::default();
        if enums.contains_key(CMD_ENUM_NAME) {
            for entry in enums.get(CMD_ENUM_NAME).unwrap().entries() {
                commands.insert(entry.name().to_string(), Command::from_enum_entry(entry));
            }
        }
        commands
    }

    fn retain_mav_cmd_entries(&mut self, command_names: &HashSet<String>) {
        if let Some(mav_cmd) = self.enums.get(CMD_ENUM_NAME) {
            let mut entries: HashMap<String, &EnumEntry> = HashMap::new();

            for cmd_name in command_names {
                match cmd_name.as_str() {
                    "*" => {
                        for entry in mav_cmd.entries() {
                            entries.insert(entry.name().to_string(), entry);
                        }
                    }
                    wildcard if cmd_name.ends_with('*') => {
                        let prefix = wildcard.trim_end_matches('*');
                        for entry in mav_cmd.entries() {
                            if entry.name().starts_with(prefix) {
                                entries.insert(entry.name().to_string(), entry);
                            }
                        }
                    }
                    cmd_name => {
                        if let Some(entry) = mav_cmd.get_entry_by_name(cmd_name) {
                            entries.insert(entry.name().to_string(), entry);
                        }
                    }
                }
            }

            let entries: Vec<EnumEntry> = entries.values().map(|&entry| entry.clone()).collect();

            self.enums.insert(
                CMD_ENUM_NAME.into(),
                mav_cmd.to_builder().set_entries(entries.as_slice()).build(),
            );
        }
    }
}

impl Buildable for Dialect {
    type Builder = DialectBuilder;

    /// Creates [`DialectBuilder`] initialised with current dialect.
    fn to_builder(&self) -> Self::Builder {
        Self::Builder {
            dialect: self.clone(),
        }
    }
}

/// [`Builder`] for [`Dialect`].
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct DialectBuilder {
    dialect: Dialect,
}

impl Builder for DialectBuilder {
    type Buildable = Dialect;

    fn build(&self) -> Self::Buildable {
        // We need this to get error when `Dialect` changes
        #[allow(clippy::match_single_binding)]
        match self.dialect.clone() {
            Dialect {
                name,
                canonical_name,
                version,
                dialect,
                messages,
                enums,
                commands,
                includes,
                metadata,
            } => Dialect {
                name,
                canonical_name,
                version,
                dialect,
                messages,
                enums,
                commands,
                includes,
                metadata,
            },
        }
    }
}

impl DialectBuilder {
    /// Sets dialect's name.
    ///
    /// Also sets `canonical_name`.
    ///
    /// See:
    ///
    ///  - [`Dialect::name`]
    ///  - [`Dialect::canonical_name`]
    pub fn set_name(&mut self, name: impl AsRef<str>) -> &mut Self {
        self.dialect.name = name.as_ref().to_string();
        self.dialect.canonical_name = dialect_canonical_name(name.as_ref());
        self
    }

    /// Sets dialect's version.
    ///
    /// See: [`Dialect::version`].
    pub fn set_version(&mut self, version: Option<DialectVersion>) -> &mut Self {
        self.dialect.version = version;
        self
    }

    /// Sets dialect's ID.
    ///
    /// See: [`Dialect::dialect`].
    pub fn set_dialect(&mut self, dialect: Option<DialectId>) -> &mut Self {
        self.dialect.dialect = dialect;
        self
    }

    /// Inserts message to dialect.
    ///
    /// Message with the same [`Message::name`] will be replaced.
    ///
    /// See: [`Dialect::messages`].
    pub fn insert_message(&mut self, message: Message) -> &mut Self {
        self.dialect
            .messages
            .insert(message.name().to_string(), message);
        self
    }

    /// Removes message from dialect referencing it by it's name.
    ///
    /// See: [`Dialect::messages`].
    pub fn remove_message_by_name(&mut self, name: impl AsRef<str>) -> &mut Self {
        self.dialect.messages.remove(name.as_ref());
        self
    }

    /// Inserts enum to dialect.
    ///
    /// Enum with the same [`Enum::name`] will be replaced.
    ///
    /// See: [`Dialect::enums`].
    pub fn insert_enum(&mut self, mav_enum: Enum) -> &mut Self {
        self.dialect
            .enums
            .insert(mav_enum.name().to_string(), mav_enum);
        self.dialect.commands = Dialect::derive_commands_from_enums(&self.dialect.enums);
        self
    }

    /// Removes enum from dialect referencing it by it's name.
    ///
    /// See: [`Dialect::enums`].
    pub fn remove_enum_by_name(&mut self, name: impl AsRef<str>) -> &mut Self {
        self.dialect.messages.remove(name.as_ref());
        self.dialect.commands = Dialect::derive_commands_from_enums(&self.dialect.enums);
        self
    }

    /// Sets dialect metadata.
    ///
    /// See: [`Dialect::metadata`]
    pub fn set_metadata(&mut self, metadata: DialectMetadata) -> &mut Self {
        self.dialect.metadata = metadata;
        self
    }
}

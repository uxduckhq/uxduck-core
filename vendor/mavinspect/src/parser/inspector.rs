use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use quick_xml::reader::Reader;

use crate::protocol::{
    Dialect, DialectId, DialectVersion, Enum, Message, MessageField, MessageId, Protocol,
};

use super::definition::DialectXmlDefinition;
use super::errors::{XmlInspectionError, XmlParseError};
use super::xml::XmlParser;
use crate::errors::Result;
use crate::parser::metadata::MavInspectMetadata;
use crate::protocol::DialectMetadata;
use crate::utils::{dialect_canonical_name, Buildable, Builder};

/// Discovers and parses MAVLink XML definitions.
///
/// # Usage
///
/// Instead of direct instantiation we use [builder](https://rust-unofficial.github.io/patterns/patterns/creational/builder.html)
/// pattern:
///
/// ```rust
/// use mavinspect::parser::Inspector;
///
/// let inspector = Inspector::builder()
///     .add_source("./message_definitions/extra")
///     // ...
///     .build().unwrap();
/// ```
///
/// Here we call [`Inspector::builder`] to create an instance of [`InspectorBuilder`], apply desired
/// configuration, and then call [`InspectorBuilder`] to obtain an instance of [`XmlInspectionError`].
///
/// Once [`Inspector`] is instantiated, use [`Inspector::parse`] to parse MAVLink XML definitions from configured
/// sources.
///
/// # Examples
///
/// Load dialects from `./message_definitions/standard` and get `HEARTBEAT_MESSAGE` from `minimal`
/// dialect:
///
/// ```rust
/// use mavinspect::parser::Inspector;
///
/// // Instantiate inspector and load list of XML definitions
/// let inspector = Inspector::builder()
///     .set_sources(&[
///         // Standard definitions from
///         // https://github.com/mavlink/mavlink/tree/master/message_definitions/v1.0
///         "./message_definitions/standard",
///         // Extra definitions which depend on standard dialects
///         "./message_definitions/extra",
///     ])
///     // Include only following dialects
///     .set_include(&["CrazyFlight", "common"])
///     .build().unwrap();
///
/// // Parse all XML definitions
/// let protocol = inspector.parse().unwrap();
///   
/// // Get `CrazyFlight` custom-defined dialect
/// let crazy_flight = protocol.get_dialect_by_canonical_name("crazy_flight").unwrap();
///
/// // Get `CRAZYFLIGHT_OUTCRY` message
/// let outcry_message = crazy_flight.get_message_by_name("CRAZYFLIGHT_OUTCRY").unwrap();
/// println!("\n`CRAZYFLIGHT_OUTCRY` message: {:#?}", outcry_message);
///
/// // Get `HEARTBEAT` message which custom dialect inherits from `standard` dialect
/// let heartbeat_message = crazy_flight.get_message_by_name("HEARTBEAT").unwrap();
/// // Verify that `HEARTBEAT` message is defined in `minimal` dialect
/// assert_eq!(heartbeat_message.defined_in(), "minimal");
/// ```
///
/// # Dialect Naming Collisions
///
/// Upon discovery, dialects with the same [canonical name](DialectXmlDefinition::canonical_name) will be considered
/// equal and will cause naming collision.
///
/// As a dialect canonical name we use a `snake_case` version of its XML definition file basename (without extension).
/// Which means that `MyDialect`, `my_dialect`, and `my-dialect` will be considered the same and have a canonical name
/// `my_dialect`. At the same time, `mydialect` will be considered as a different dialect.
///
/// You can use [`dialect_canonical_name`] utility function to obtain canonical name for a dialect.
///
/// # Metadata
///
/// Upon reading from a location that contains MAVlink XML definitions, [`Inspector`] looks up for
/// a file `.dialects-metadata.yml`. If it exists, it loads its contents into a [`DialectMetadata`]
/// and attaches to all top-level dialects loaded from this location (includes are ignored).
#[derive(Debug)]
pub struct Inspector {
    definitions: Vec<DialectXmlDefinition>,
}

/// Configures and builds an instance of [`Inspector`].
///
/// # Fields
///
/// ### `sources`
///
/// List of paths to directories with `MAVLink` XML definitions. Use [`InspectorBuilder::set_sources`] to set sources
/// or [`InspectorBuilder::add_source`] to push singe source location.
///
/// ### [`include`](InspectorBuilder::set_include) / [`exclude`](InspectorBuilder::set_exclude)
///
/// Define:
///
/// * `include`: [`InspectorBuilder::set_include`]
/// * `exclude`: [`InspectorBuilder::set_exclude`]
///
/// These fields specify which dialects will be added. Exclusion/inclusion rules will be applied
/// only to the first level of dialects. Which means that if dialect `X` has dialect `Y` as a
/// dependency, then `Y` will be loaded and parsed even if explicitly excluded.  
///
/// If both [`include`](InspectorBuilder::set_include) and [`exclude`](InspectorBuilder::set_exclude)
/// are set, then includes will have precedence over exclusion list. Which means that dialects will
/// be excluded within the list of explicitly specified dialects.
#[derive(Clone, Debug, Default)]
pub struct InspectorBuilder {
    sources: Vec<PathBuf>,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
}

impl InspectorBuilder {
    /// Load an instance of [`Inspector`].
    ///
    /// Loads a collection of [`DialectXmlDefinition`] in the process.
    ///
    /// # Errors
    ///
    /// * Returns variants of [`Error`](crate::errors::Error) if XML definitions discovery fails.
    /// * In the case of dialect naming collision returns [`XmlInspectionError::NamingCollision`].
    pub fn build(&self) -> Result<Inspector> {
        let sources: Vec<&Path> = self.sources.iter().map(|p| p.as_path()).collect();
        let mut definitions = Inspector::discover(&sources)?;

        if let Some(names) = &self.include {
            log::info!("Only the following dialects will be included: {names:?}");
            let names: Vec<String> = names
                .iter()
                .map(|name| dialect_canonical_name(name))
                .collect();
            definitions.retain(|def| names.contains(&dialect_canonical_name(def.name())));
        }

        if let Some(names) = &self.exclude {
            log::info!("The following dialects will be excluded: {names:?}");
            let names: Vec<String> = names
                .iter()
                .map(|name| dialect_canonical_name(name))
                .collect();
            definitions.retain(|def| !names.contains(&dialect_canonical_name(def.name())));
        }

        Ok(Inspector { definitions })
    }

    /// Sets source paths.
    ///
    /// Each `sources` item is a path to a directory containing `MAVLink` XML definitions.
    pub fn set_sources<T>(&mut self, sources: &[T]) -> &mut Self
    where
        T: Into<PathBuf> + Clone,
    {
        self.sources = sources.iter().cloned().map(|src| src.into()).collect();
        self
    }

    /// Adds source path.
    ///
    /// Adds path to a directory containing `MAVLink` XML definitions.
    pub fn add_source<T>(&mut self, source: T) -> &mut Self
    where
        T: Into<PathBuf> + Clone,
    {
        self.sources.push(source.into());
        self
    }

    /// Sets the list of dialect names to include.
    ///
    /// Names will be transformed to [`Dialect::canonical_name`] upon matching.
    pub fn set_include<T: ?Sized + ToString>(&mut self, dialect_names: &[&T]) -> &mut Self {
        self.include = Some(dialect_names.iter().map(|s| s.to_string()).collect());
        self
    }

    /// Sets the list of dialect names to exclude.
    ///
    /// Names will be transformed to [`Dialect::canonical_name`] upon matching.
    pub fn set_exclude<T: ?Sized + ToString>(&mut self, dialect_names: &[&T]) -> &mut Self {
        self.exclude = Some(dialect_names.iter().map(|s| s.to_string()).collect());
        self
    }
}

/// MAVLink protocol parser.
impl Inspector {
    /// Instantiates builder for [`Inspector`].
    pub fn builder() -> InspectorBuilder {
        InspectorBuilder::default()
    }

    /// List of loaded dialect definitions which can be parsed.
    pub fn definitions(&self) -> &[DialectXmlDefinition] {
        &self.definitions
    }

    /// Parse XML definitions.
    ///
    /// Parses [`Self::definitions`] and returns an instance of [`Protocol`] that contains all parsed dialects.
    ///
    /// # Errors
    ///
    /// Returns [`XmlParseError`] for invalid dialect definitions.
    pub fn parse(&self) -> Result<Protocol> {
        let mut dialects: HashMap<String, Dialect> = HashMap::new();

        log::info!("Parsing dialects.");

        // Parsing started
        let started_at = Instant::now();

        // Iterate through definitions and parse dialects
        for def in &self.definitions {
            // Do nothing if this dialect has already been parsed
            if dialects.contains_key(&def.canonical_name()) {
                continue;
            }

            Self::parse_definition(def, &mut dialects)?;
        }

        // Calculate parsing duration
        let ended_at = Instant::now();
        let duration = ended_at - started_at;

        log::info!("All dialects parsed.");
        log::info!("Parsed dialects: {:?}", dialects.keys());
        log::info!(
            "Parse duration: {}s",
            (duration.as_micros() as f64) / 1000000.0
        );

        Ok(Protocol::new(dialects.values().cloned().collect()))
    }

    /// Returns a list of dialect names within a collection of paths.
    ///
    /// The function attempts to parse all dialect definitions and discovers all dialect
    /// dependencies. Which means that the returned list may include dialects outside the specified
    /// paths.
    ///
    /// <section class="warning">
    /// The function is slow since it parses XML definitions. The good news is that you will catch
    /// all errors related to dialect parsing.
    /// </section>
    ///
    /// To obtain canonical dialect names use [`Inspector::discover_dialect_canonical_names`].
    ///
    /// ## Examples
    ///
    /// ```rust
    /// use mavinspect::parser::Inspector;
    ///
    /// let names = Inspector::discover_dialect_names(&[
    ///         // Standard definitions from
    ///         // https://github.com/mavlink/mavlink/tree/master/message_definitions/v1.0
    ///         "./message_definitions/standard",
    ///         // Extra definitions which depend on standard dialects
    ///         "./message_definitions/extra",
    ///     ]).unwrap();
    ///
    /// assert!(names.contains(&"common".to_string()));
    /// assert!(names.contains(&"ASLUAV".to_string()));
    /// assert!(names.contains(&"MAVInspect_test".to_string()));
    /// ```
    pub fn discover_dialect_names<T: ?Sized + AsRef<OsStr>>(paths: &[&T]) -> Result<Vec<String>> {
        let definitions = Self::discover(paths)?;
        let canonical_names = definitions
            .iter()
            .map(|def| def.name().to_string())
            .collect();
        Ok(canonical_names)
    }

    /// Returns a list of dialect canonical names within a collection of paths.
    ///
    /// The function attempts to parse all dialect definitions and discovers all dialect
    /// dependencies. Which means that the returned list may include dialects outside the specified
    /// paths.
    ///
    /// <section class="warning">
    /// The function is slow since it parses XML definitions. The good news is that you will catch
    /// all errors related to dialect parsing.
    /// </section>
    ///
    /// Learn more about [canonical names](Dialect::canonical_name).
    ///
    /// To obtain regular dialect names (as provided by file names) use [`Inspector::discover_dialect_names`].
    ///
    /// ## Examples
    ///
    /// ```rust
    /// use mavinspect::parser::Inspector;
    ///
    /// let canonical_names = Inspector::discover_dialect_canonical_names(&[
    ///         // Standard definitions from
    ///         // https://github.com/mavlink/mavlink/tree/master/message_definitions/v1.0
    ///         "./message_definitions/standard",
    ///         // Extra definitions which depend on standard dialects
    ///         "./message_definitions/extra",
    ///     ]).unwrap();
    ///
    /// assert!(canonical_names.contains(&"common".to_string()));
    /// assert!(canonical_names.contains(&"asluav".to_string()));
    /// assert!(canonical_names.contains(&"mav_inspect_test".to_string()));
    /// ```
    pub fn discover_dialect_canonical_names<T: ?Sized + AsRef<OsStr>>(
        paths: &[&T],
    ) -> Result<Vec<String>> {
        let definitions = Self::discover(paths)?;
        let canonical_names = definitions.iter().map(|def| def.canonical_name()).collect();
        Ok(canonical_names)
    }

    fn discover<T: ?Sized + AsRef<OsStr>>(paths: &[&T]) -> Result<Vec<DialectXmlDefinition>> {
        // Found dialects
        let mut dialects: Vec<DialectXmlDefinition> = Vec::new();
        // A set of dialect IDs. New dialects with existing IDs will be rejected
        let mut dialect_ids: HashMap<String, String> = HashMap::new();

        // Iterate over paths
        for path in paths {
            let directory_path = Path::new(&path).canonicalize()?;
            log::debug!(
                "Entering XML definitions directory: {}",
                directory_path
                    .to_str()
                    .ok_or(XmlInspectionError::InvalidPath)?
            );

            // Iterate over files within directory
            for entry in fs::read_dir(directory_path)? {
                let entry_path = entry?.path();

                // Filter only XML files
                if entry_path.is_file()
                    && entry_path
                    .extension()
                    .unwrap_or_default()
                    .to_str()
                    .unwrap_or_default()
                    .to_lowercase()
                    .eq("xml")
                {
                    let path = entry_path
                        .to_str()
                        .ok_or(XmlInspectionError::InvalidPath)?
                        .to_string();
                    let definition = DialectXmlDefinition::load_from_path(&path);

                    // Check for naming collisions
                    #[allow(clippy::map_entry)]
                    if dialect_ids.contains_key(&definition.canonical_name()) {
                        return Err(XmlInspectionError::NamingCollision {
                            first: definition.name().to_string(),
                            second: dialect_ids
                                .get(&definition.canonical_name())
                                .unwrap()
                                .clone(),
                            canonical: definition.canonical_name(),
                        }
                            .into());
                    } else {
                        dialect_ids
                            .insert(definition.canonical_name(), definition.name().to_string());
                    }

                    dialects.push(definition);
                }
            }
        }

        Ok(dialects)
    }

    fn parse_definition(
        definition: &DialectXmlDefinition,
        dialects: &mut HashMap<String, Dialect>,
    ) -> Result<()> {
        if dialects.contains_key(&definition.canonical_name()) {
            return Ok(());
        }

        let metadata = Self::load_metadata(definition);

        let mut enums: HashMap<String, Enum> = HashMap::new();
        let mut messages: HashMap<MessageId, Message> = HashMap::new();
        Self::load_dependencies(definition, dialects, &mut enums, &mut messages)?;

        let started_at = Instant::now();

        let mut parser: XmlParser = XmlParser::new(&mut enums, &mut messages);
        let mut file_reader = Reader::from_file(definition.path()).unwrap();
        parser.parse(definition.name(), &mut file_reader)?;

        Self::update_messages_defined_in(
            &mut messages,
            &mut enums,
            definition.canonical_name().as_str(),
        );
        Self::validate_field_enum_types(&enums, &messages)?;

        let version = Self::derive_dialect_version(definition);
        let dialect_id = Self::derive_dialect_id(definition);
        let includes = Self::derive_includes(definition);

        dialects.insert(
            definition.canonical_name(),
            Dialect::new(
                definition.name(),
                version,
                dialect_id,
                messages.values().cloned().collect(),
                enums.values().cloned().collect(),
                includes,
                metadata,
            ),
        );

        Self::report_definition_parsing(definition, started_at, Instant::now());
        Ok(())
    }

    fn load_dependencies(
        definition: &DialectXmlDefinition,
        dialects: &mut HashMap<String, Dialect>,
        enums: &mut HashMap<String, Enum>,
        messages: &mut HashMap<MessageId, Message>,
    ) -> Result<()> {
        for dependency in definition.includes() {
            Self::parse_definition(dependency, dialects)?;
        }

        Self::merge_enums(definition, dialects, enums);
        Self::merge_messages(definition, dialects, messages);

        Ok(())
    }

    fn report_definition_parsing(
        definition: &DialectXmlDefinition,
        started_at: Instant,
        finished_at: Instant,
    ) {
        let duration = finished_at - started_at;
        if log::log_enabled!(log::Level::Debug) {
            log::debug!("Parsed definition '{}'.", definition.name());
            log::debug!("Definition path: {}", definition.path());
            log::debug!("Definition version: {:?}", definition.version());
            log::debug!("Definition dialect #: {:?}", definition.dialect());
            log::debug!(
                "Parse duration: {}s",
                (duration.as_micros() as f64) / 1000000.0
            );
        }
    }

    fn derive_dialect_version(definition: &DialectXmlDefinition) -> Option<DialectVersion> {
        let mut version = definition.version();
        if version.is_none() {
            for dependency in definition.includes() {
                if dependency.version().is_some() {
                    version = dependency.version();
                }
            }
        }
        version
    }

    fn derive_dialect_id(definition: &DialectXmlDefinition) -> Option<DialectId> {
        let mut dialect_id = definition.dialect();
        if dialect_id.is_none() {
            for dependency in definition.includes() {
                if dependency.version().is_some() {
                    dialect_id = dependency.dialect();
                }
            }
        }
        dialect_id
    }

    fn derive_includes(definition: &DialectXmlDefinition) -> Vec<String> {
        let mut includes = Vec::new();
        for def in definition.includes() {
            includes.push(def.canonical_name().to_string());
        }
        includes
    }

    fn validate_field_enum_types(
        enums: &HashMap<String, Enum>,
        messages: &HashMap<MessageId, Message>,
    ) -> Result<()> {
        for mav_enum in enums.values() {
            for msg in messages.values() {
                for field in msg.fields() {
                    Self::validate_field_enum_type(mav_enum, field)?;
                }
            }
        }
        Ok(())
    }

    fn validate_field_enum_type(mav_enum: &Enum, field: &MessageField) -> Result<()> {
        if let Some(field_enum_name) = field.r#enum() {
            if field_enum_name == mav_enum.name() && *field.r#type() < mav_enum.inferred_type() {
                return Err(XmlParseError::MessageFieldISTooSmallForEnum {
                    enum_name: mav_enum.name().into(),
                    enum_type: mav_enum.inferred_type(),
                    field_name: field.name().into(),
                    field_type: field.r#type().clone(),
                }
                    .into());
            }
        }
        Ok(())
    }

    fn merge_enums(
        definition: &DialectXmlDefinition,
        dialects: &mut HashMap<String, Dialect>,
        enums: &mut HashMap<String, Enum>,
    ) {
        let mut enums_variants_map: HashMap<&str, Vec<&Enum>> = Default::default();
        for dependency in definition.includes() {
            let dialect = dialects.get(&dependency.canonical_name()).unwrap();

            for enm in dialect.enums() {
                if let Some(variants) = enums_variants_map.get_mut(enm.name()) {
                    for &variant in variants.iter() {
                        if variant.defined_in() == enm.defined_in() {
                            continue;
                        }
                    }
                    variants.push(enm);
                } else {
                    enums_variants_map.insert(enm.name(), vec![enm]);
                }
            }
        }

        for (enum_name, variants) in enums_variants_map {
            let enm = Self::merged_enum(variants.as_slice(), definition.canonical_name().as_str());
            enums.insert(enum_name.to_string(), enm);
        }
    }

    fn merge_messages(
        definition: &DialectXmlDefinition,
        dialects: &mut HashMap<String, Dialect>,
        messages: &mut HashMap<MessageId, Message>,
    ) {
        let mut messages_variants_map: HashMap<MessageId, Vec<&Message>> = Default::default();
        for dependency in definition.includes() {
            let dialect = dialects.get(&dependency.canonical_name()).unwrap();

            for msg in dialect.messages() {
                if let Some(msg_variants) = messages_variants_map.get_mut(&msg.id()) {
                    for &variant in msg_variants.iter() {
                        if variant.defined_in() == msg.defined_in() {
                            continue;
                        }
                    }
                    msg_variants.push(msg);
                } else {
                    messages_variants_map.insert(msg.id(), vec![msg]);
                }
            }
        }

        for (msg_id, variants) in messages_variants_map {
            let msg = Self::merged_message(variants.as_slice());
            messages.insert(msg_id, msg);
        }
    }

    fn merged_enum(variants: &[&Enum], dialect_canonical_name: &str) -> Enum {
        let mut enum_builder = Enum::builder();

        let mut entries = HashMap::new();

        for &enm in variants {
            enum_builder.set_name(enm.name());
            enum_builder.set_description(enm.description());
            enum_builder.set_bitmask(enm.bitmask());
            enum_builder.set_deprecated(enm.deprecated().cloned());
            enum_builder.set_defined_in(enm.defined_in());

            for entry in enm.entries() {
                entries.insert(entry.value(), entry.clone());
            }
        }

        let entry_values: HashSet<u32> = HashSet::from_iter(entries.keys().copied());
        let entries = entries.values().cloned().collect::<Vec<_>>();
        enum_builder.set_entries(entries.as_slice());

        if variants.len() > 1 {
            let mut first_with_all_entries = None;

            for &enm in variants {
                let enum_entry_values =
                    HashSet::from_iter(enm.entries().iter().map(|entry| entry.value()));

                if enum_entry_values.is_superset(&entry_values) {
                    first_with_all_entries = Some(enm.defined_in());
                    break;
                }
            }

            enum_builder.set_defined_in(first_with_all_entries.unwrap_or(dialect_canonical_name));
        }

        enum_builder.build()
    }

    fn merged_message(messages: &[&Message]) -> Message {
        let mut message = messages.last().cloned().unwrap().clone();

        let mut appears_in: HashSet<String> =
            HashSet::from_iter(message.appears_in().iter().map(|s| s.as_ref().to_string()));

        for &msg in messages {
            let msg_appears_in: HashSet<String> =
                HashSet::from_iter(msg.appears_in().iter().map(|s| s.as_ref().to_string()));
            if msg_appears_in.is_superset(&appears_in) {
                message = msg.clone();
                appears_in = msg_appears_in;
            }
        }

        message
    }

    fn update_messages_defined_in(
        messages: &mut HashMap<MessageId, Message>,
        enums: &mut HashMap<String, Enum>,
        dialect_canonical_name: &str,
    ) {
        let msg_ids = messages.keys().cloned().collect::<Vec<_>>();

        'messages: for msg_id in msg_ids {
            if let Some(message) = messages.get(&msg_id) {
                if message.defined_in() == dialect_canonical_name {
                    continue 'messages;
                }

                for field in message.fields() {
                    if let Some(field_enum_name) = field.r#enum() {
                        if let Some(field_enum) = enums.get(&field_enum_name.to_string()) {
                            if field_enum.defined_in() == dialect_canonical_name {
                                let updates_message = message
                                    .to_builder()
                                    .set_defined_in(dialect_canonical_name)
                                    .build();
                                messages.insert(msg_id, updates_message);
                                continue 'messages;
                            }
                        }
                    }
                }
            }
        }
    }

    fn load_metadata(definition: &DialectXmlDefinition) -> DialectMetadata {
        if let Some(directory) = PathBuf::from(definition.path()).parent() {
            let metadata_path = directory.join(".dialects-metadata.yml");
            if let Ok(contents) = fs::read_to_string(metadata_path) {
                if let Ok(metadata) = serde_saphyr::from_str(&contents) {
                    let metadata: MavInspectMetadata = metadata;
                    return metadata.metadata_for_dialect(definition.name());
                }
            }
        }
        DialectMetadata::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_dialect_paths() -> Vec<&'static str> {
        vec![
            "./message_definitions/standard",
            "./message_definitions/extra",
        ]
    }

    /// Tests that MAVLink message definitions available ot default path.
    #[test]
    fn dialects_are_available() {
        let parser = Inspector::builder()
            .set_sources(&default_dialect_paths())
            .build()
            .unwrap();

        assert!(!parser.definitions().is_empty());
    }

    /// Tests that MAVLink message definitions available ot default path.
    #[test]
    fn builder_can_add_sources() {
        let parser = Inspector::builder()
            .add_source("./message_definitions/standard")
            .build()
            .unwrap();

        assert!(!parser.definitions().is_empty());
    }

    /// Tests that inclusion rules work.
    #[test]
    fn inclusion_rules() {
        let parser = Inspector::builder()
            .set_sources(&default_dialect_paths())
            .set_include(&["CrazyFlight"])
            .build()
            .unwrap();

        assert!(!parser.definitions().is_empty());
        assert_eq!(parser.definitions().len(), 1);
        assert_eq!(parser.definitions()[0].name(), "CrazyFlight");
    }

    /// Tests that exclusion rules work.
    #[test]
    fn exclusion_rules() {
        let parser = Inspector::builder()
            .set_sources(&default_dialect_paths())
            .set_exclude(&["CrazyFlight"])
            .build()
            .unwrap();

        assert!(!parser.definitions().is_empty());

        for def in parser.definitions() {
            assert_ne!(def.name(), "CrazyFlight");
        }
    }

    /// Tests that inclusion rules work with canonical names.
    #[test]
    fn inclusion_by_canonical_names() {
        let parser = Inspector::builder()
            .set_sources(&default_dialect_paths())
            .set_include(&["crazy_flight"])
            .build()
            .unwrap();

        assert!(!parser.definitions().is_empty());
        assert_eq!(parser.definitions().len(), 1);
        assert_eq!(parser.definitions()[0].name(), "CrazyFlight");
    }

    /// Tests that exclusion rules work with canonical names.
    #[test]
    fn exclusion_by_canonical_name() {
        let parser = Inspector::builder()
            .set_sources(&default_dialect_paths())
            .set_exclude(&["crazy_flight"])
            .build()
            .unwrap();

        assert!(!parser.definitions().is_empty());

        for def in parser.definitions() {
            assert_ne!(def.name(), "CrazyFlight");
        }
    }
}

use std::collections::HashMap;

use crc_any::CRCu64;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::protocol::{Dialect, DialectTags, Filter, Fingerprint};

/// MAVLink protocol.
///
/// [`Protocol`] is a collection of MAVLink dialects. Each [`Dialect`] contains MAVLink [`enums`](Dialect::enums) and
/// [`messages`](Dialect::messages) as well as additional information (like version or dependencies).
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[cfg_attr(feature = "specta", specta(rename = "MavInspectProtocol"))]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Protocol {
    dialects: HashMap<String, Dialect>,
    default_dialect_canonical_name: Option<String>,
}

impl Protocol {
    /// Default constructor.
    pub fn new(dialects: Vec<Dialect>) -> Self {
        let dialects = {
            let mut dialects_: HashMap<String, Dialect> = HashMap::new();
            for dialect in dialects {
                dialects_.insert(dialect.canonical_name().to_string(), dialect);
            }
            dialects_
        };
        Self {
            dialects,
            default_dialect_canonical_name: None,
        }
    }

    /// Replaces or sets the default dialect using the specified canonical name.
    ///
    /// If dialect with `canonical_name` does not exist, then nothing will be changed.
    ///
    /// See: [Dialect::canonical_name]
    pub fn with_default_dialect<S: AsRef<str>>(self, canonical_name: S) -> Self {
        if !self.contains_dialect_with_canonical_name(canonical_name.as_ref()) {
            return self;
        }
        Self {
            default_dialect_canonical_name: Some(canonical_name.as_ref().to_string()),
            ..self
        }
    }

    /// Keeps only specified dialects.
    ///
    /// If `keep_deps` is set to `true`, the function keeps dialect dependencies. Which means that
    /// if specified dialects `A` has a dependency `B`, then `B` will be kept as well.
    ///
    /// If [`default_dialect`](Self::default_dialect) was previously set and no longer in included
    /// dialects, then it will be dropped and protocol will no longer have a default designated
    /// dialect.
    ///
    /// <section class="warning">
    /// Filtering with `keep_deps` set to `false` may lead to undesired behavior since entities
    /// within a protocol may point to dialects which are no longer present. All tools within
    /// [Mavka](https://mavka.gitlab.io/home/) toolchain **always** preserve dialect dependencies.
    /// </section>
    ///
    /// If `keep_tags` is set, then dialects with specified tags will be kept.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use mavinspect::parser::Inspector;
    ///
    /// let inspector = Inspector::builder()
    ///     .set_sources(&[
    ///         // Standard definitions from
    ///         // https://github.com/mavlink/mavlink/tree/master/message_definitions/v1.0
    ///         "./message_definitions/standard",
    ///         // Extra definitions which depend on standard dialects
    ///         "./message_definitions/extra",
    ///     ])
    ///     .build()
    ///     .unwrap();
    /// let protocol = inspector.parse().unwrap();
    ///
    /// // Filter keeping dialect dependencies
    /// let protocol = protocol.with_dialects_included(&["common"], true, None);
    ///
    /// // Should have these
    /// assert!(protocol.contains_dialect_with_canonical_name("common"));
    /// assert!(protocol.contains_dialect_with_canonical_name("standard"));
    /// assert!(protocol.contains_dialect_with_canonical_name("minimal"));
    /// // Should not have these
    /// assert!(!protocol.contains_dialect_with_canonical_name("development"));
    /// assert!(!protocol.contains_dialect_with_canonical_name("all"));
    ///
    /// // Filter excluding dialect dependencies (may be dangerous)
    /// let protocol = protocol.with_dialects_included(&["common"], false, None);
    ///
    /// // Should have these
    /// assert!(protocol.contains_dialect_with_canonical_name("common"));
    /// // Should not have these
    /// assert!(!protocol.contains_dialect_with_canonical_name("standard"));
    /// assert!(!protocol.contains_dialect_with_canonical_name("minimal"));
    /// ```
    ///
    /// Default dialect will be dropped if necessary:
    ///
    /// ```rust
    /// use mavinspect::parser::Inspector;
    ///
    /// let inspector = Inspector::builder()
    ///     .set_sources(&[
    ///         // Standard definitions from
    ///         // https://github.com/mavlink/mavlink/tree/master/message_definitions/v1.0
    ///         "./message_definitions/standard",
    ///         // Extra definitions which depend on standard dialects
    ///         "./message_definitions/extra",
    ///     ])
    ///     .build()
    ///     .unwrap();
    /// let protocol = inspector.parse().unwrap().with_default_dialect("all");
    ///
    /// // Now we have default dialect set to `all`
    /// assert_eq!(protocol.default_dialect().unwrap().canonical_name(), "all");
    ///
    /// // Filter dialects
    /// let protocol = protocol.with_dialects_included(&["common"], true, None);
    ///
    /// // No more default dialect
    /// assert!(protocol.default_dialect().is_none());
    /// ```
    pub fn with_dialects_included<T: AsRef<str>>(
        self,
        canonical_names: &[T],
        keep_deps: bool,
        keep_tags: Option<DialectTags>,
    ) -> Self {
        let keep_tags = keep_tags.unwrap_or_default();

        let to_keep = if keep_deps {
            let mut requested: Vec<&str> =
                canonical_names.iter().map(|name| name.as_ref()).collect();
            let mut to_keep: Vec<&str> = Vec::new();

            let mut completed: bool = false;
            while !completed {
                completed = true;
                for (name, dialect) in self.dialects.iter() {
                    let should_keep = requested.contains(&name.as_str())
                        || dialect.metadata().has_one_of_tags(&keep_tags);
                    let not_processed = !to_keep.contains(&name.as_str());

                    if should_keep && not_processed {
                        to_keep.push(name.as_str());
                        for dep_name in dialect.includes() {
                            requested.push(dep_name);
                        }
                        completed = false;
                    }
                }
            }

            to_keep
        } else {
            canonical_names.iter().map(|name| name.as_ref()).collect()
        };

        let dialects = HashMap::from_iter(
            self.dialects
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .filter(|(name, _)| to_keep.contains(&name.as_ref())),
        );

        let default_dialect_canonical_name = self
            .default_dialect_canonical_name
            .filter(|name| to_keep.contains(&name.as_str()));

        Self {
            dialects,
            default_dialect_canonical_name,
        }
    }

    /// Dialects within protocol.
    pub fn dialects(&self) -> impl Iterator<Item=&Dialect> {
        self.dialects.values()
    }

    /// Returns a default dialect if there is a designated one.
    ///
    /// Default dialects are useful for building APIs that rely on a specific (usually, the most
    /// feature-rich dialect).
    ///
    /// See: [`Self::is_default_dialect`] and [`Self::is_default_dialect_canonical_name`].
    pub fn default_dialect(&self) -> Option<&Dialect> {
        let canonical_name = self.default_dialect_canonical_name.as_ref()?;
        self.get_dialect_by_canonical_name(canonical_name)
    }

    /// Checks that provided dialect is indeed a default one.
    ///
    /// <section class="warning">
    /// The check is not exhaustive!
    ///
    /// We only use [`Dialect::canonical_name`].
    /// </section>
    ///
    /// See: [`Self::default_dialect`] and [`Self::is_default_dialect_canonical_name`].
    pub fn is_default_dialect(&self, dialect: &Dialect) -> bool {
        self.default_dialect_canonical_name
            .as_ref()
            .map(|default_dialect_canonical_name| {
                default_dialect_canonical_name.as_str() == dialect.canonical_name()
            })
            .unwrap_or(false)
    }

    /// Checks that provided dialect [canonical](Dialect::canonical_name) name matches the default
    /// dialect.
    ///
    /// See: [`Self::default_dialect`] and [`Self::is_default_dialect`]
    pub fn is_default_dialect_canonical_name(&self, dialect_canonical_name: &str) -> bool {
        self.default_dialect_canonical_name
            .as_ref()
            .map(|default_dialect_canonical_name| {
                default_dialect_canonical_name.as_str() == dialect_canonical_name
            })
            .unwrap_or(false)
    }

    /// Checks that dialect with specified `name` is present in protocol.
    pub fn contains_dialect_with_name(&self, name: &str) -> bool {
        self.dialects.values().any(|dialect| dialect.name() == name)
    }

    /// Checks that dialect with specified [canonical](Dialect::canonical_name) name is present in
    /// protocol.
    ///
    /// Canonical name is defined by [`Dialect::canonical_name`].
    pub fn contains_dialect_with_canonical_name(&self, canonical_name: &str) -> bool {
        self.dialects.contains_key(canonical_name)
    }

    /// Get dialect with specified `name`.
    pub fn get_dialect_by_name(&self, name: &str) -> Option<&Dialect> {
        self.dialects
            .values()
            .find(|dialect| dialect.name() == name)
    }

    /// Get dialect with specified `canonical_name`.
    ///
    /// Canonical name is defined by [`Dialect::canonical_name`].
    pub fn get_dialect_by_canonical_name(&self, canonical_name: &str) -> Option<&Dialect> {
        self.dialects.get(canonical_name)
    }

    /// Check dialects for emptiness.
    pub fn dialects_are_empty(&self) -> bool {
        self.dialects.is_empty()
    }

    /// Create a new instance of [`Protocol`] with dialects containing only entities matching [`Filter`].
    ///
    /// This is an immutable cloning version [`Self::retain`]. The latter filters out messages and enums
    /// in-place.
    ///
    /// See also [`Dialect::filtered`] which performs the same operation on the [`Dialect`] level.
    pub fn filtered(&self, filter: &Filter) -> Self {
        let mut protocol = Protocol::default();

        for (id, dialect) in &self.dialects {
            protocol
                .dialects
                .insert(id.clone(), dialect.filtered(filter));
        }

        protocol
    }

    /// Retain only entities matching [`Filter`] for each [`Dialect`].
    ///
    /// This is an in-place version of [`Self::filtered`]. The latter creates a new instance of
    /// [`Protocol`].
    ///
    /// See also [`Dialect::retain`] which performs the same operation on the [`Dialect`] level.
    pub fn retain(&mut self, filter: &Filter) {
        for dialect in &mut self.dialects.values_mut() {
            dialect.retain(filter);
        }
    }

    /// Protocol fingerprint.
    ///
    /// A value of opaque type [`Fingerprint`] that contains the entire protocol CRC.
    pub fn fingerprint(&self) -> Fingerprint {
        let mut crc_calculator = CRCu64::crc64();

        let mut dialects: Vec<&Dialect> = self.dialects.values().collect();
        dialects.sort_by_key(|&dialect| dialect.name().to_string());
        for dialect in dialects {
            crc_calculator.digest(&dialect.fingerprint().as_bytes());
        }

        crc_calculator.get_crc().into()
    }
}

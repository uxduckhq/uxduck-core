use crc_any::CRCu64;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use std::mem;

use crate::protocol::enums::enum_entry::EnumEntryValue;
use crate::protocol::{enums::EnumEntry, Deprecated, Description, Fingerprint, MavType};
use crate::utils::{Buildable, Builder, Named};

/// Enum
///
/// MAVLink enum is a special field type. There are two types of enums:
/// - **regular**: value specifies a particular enum option (entry)
/// - **bitmask**: each bit signifies a particular flag
///
/// Enum options (entries) and flags are specified by [`EnumEntry`].
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[cfg_attr(feature = "specta", specta(rename = "MavInspectEnum"))]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Enum {
    name: String,
    description: Description,
    entries: Vec<EnumEntry>,
    bitmask: bool,
    deprecated: Option<Deprecated>,
    defined_in: String,
    appears_in: Vec<String>,
}

impl Buildable for Enum {
    type Builder = EnumBuilder;

    /// Creates [`EnumBuilder`] initialised with current enum entry.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use mavinspect::protocol::Enum;
    /// use mavinspect::utils::{Buildable, Builder};
    ///
    /// let original = Enum::builder()
    ///     .set_name("original")
    ///     .set_description("original")
    ///     .build();
    ///
    /// let updated = original.to_builder()
    ///     .set_description("updated")
    ///     .build();
    ///
    /// assert_eq!(updated.name(), "original");
    /// assert_eq!(updated.description(), "updated");
    /// ```
    fn to_builder(&self) -> EnumBuilder {
        EnumBuilder {
            r#enum: self.clone(),
        }
    }
}

impl Named for Enum {
    fn name(&self) -> &str {
        &self.name
    }
}

impl Named for &Enum {
    fn name(&self) -> &str {
        &self.name
    }
}

impl Enum {
    /// Initiates builder.
    ///
    /// Instead of constructor we use
    /// [builder](https://rust-unofficial.github.io/patterns/patterns/creational/builder.html)
    /// pattern. An instance of [`EnumBuilder`] returned by this function is initialized
    /// with default values. Once desired values are set, you can call [`EnumBuilder::build`] to
    /// obtain [`Enum`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use mavinspect::protocol::Enum;
    /// use mavinspect::utils::Builder;
    ///
    /// let mav_enum = Enum::builder()
    ///     .set_name("name")
    ///     .set_description("description")
    ///     .build();
    ///
    /// assert!(matches!(mav_enum, Enum { .. }));
    /// assert_eq!(mav_enum.name(), "name");
    /// assert_eq!(mav_enum.description(), "description");
    /// ```
    pub fn builder() -> EnumBuilder {
        EnumBuilder::new()
    }

    /// Enum name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Enum description.
    pub fn description(&self) -> &str {
        self.description.as_str()
    }

    /// Collection of enum entries.
    pub fn entries(&self) -> &[EnumEntry] {
        self.entries.as_slice()
    }

    /// Whether this enum is a bitmask.
    pub fn bitmask(&self) -> bool {
        self.bitmask
    }

    /// Deprecation status.
    pub fn deprecated(&self) -> Option<&Deprecated> {
        self.deprecated.as_ref()
    }

    /// The [canonical](crate::protocol::dialect::Dialect::canonical_name) name of the dialect in
    /// which this enum was defined (or re-defined).
    ///
    /// If enum is defined in dialect `A` and then dialect `B` includes `A` and adds new entries,
    /// then this function will return dialect `B`. In the case when dialect `C` includes dialect
    /// `B` without redefinition of an enum, this function will still return `B`.
    ///
    /// The comprehensive list of dialects where this enum was defined can be obtained in
    /// [`Self::appears_in`].
    ///
    /// Use [`Self::was_defined_in`] to check whether enum was defined in a specific dialect.
    pub fn defined_in(&self) -> &str {
        self.defined_in.as_str()
    }

    /// Returns the list of dialect [canonical](crate::protocol::dialect::Dialect::canonical_name)
    /// names where this enum was defined.
    ///
    /// The dialect where the enum finally belongs to can be obtained in [`Self::defined_in`].
    ///
    /// Use [`Self::was_defined_in`] to check whether enum was defined in a specific dialect.
    pub fn appears_in(&self) -> &[impl AsRef<str>] {
        self.appears_in.as_slice()
    }

    /// Returns `true` if this enum was defined in a dialect with a specified
    /// [canonical](crate::protocol::dialect::Dialect::canonical_name) name.
    ///
    /// See also: [`Self::defined_in`] and [`Self::appears_in`].
    pub fn was_defined_in(&self, dialect_canonical_name: impl AsRef<str>) -> bool {
        self.appears_in
            .contains(&dialect_canonical_name.as_ref().to_string())
    }

    /// Searches for entry with the specified `name`.
    pub fn has_entry_with_name(&self, name: &str) -> bool {
        self.entries.iter().any(|entry| entry.name() == name)
    }

    /// Searches for entry with the specified `name`.
    pub fn get_entry_by_name(&self, name: &str) -> Option<&EnumEntry> {
        self.entries.iter().find(|entry| entry.name() == name)
    }

    /// Searches for entry with the specified `value`.
    pub fn get_entry_by_value(&self, value: EnumEntryValue) -> Option<&EnumEntry> {
        self.entries.iter().find(|entry| entry.value() == value)
    }

    /// Maximum value of enum given its entries.
    pub fn max_value(&self) -> EnumEntryValue {
        let mut max_value: EnumEntryValue = 0;
        for entry in self.entries.iter() {
            if entry.value() > max_value {
                max_value = entry.value()
            }
        }
        max_value
    }

    /// Enum type inferred from its maximum value.
    ///
    /// Message fields may store enum using larger types but not otherwise.
    pub fn inferred_type(&self) -> MavType {
        match self.max_value() {
            val if val <= u8::MAX as EnumEntryValue => MavType::UInt8,
            val if val <= u16::MAX as EnumEntryValue => MavType::UInt16,
            _ => MavType::UInt32,
        }
    }

    /// Enum fingerprint.
    ///
    /// A value of opaque type [`Fingerprint`] that contains enum CRC.
    pub fn fingerprint(&self) -> Fingerprint {
        let mut crc_calculator = CRCu64::crc64();

        crc_calculator.digest(format!("{:?} ", self.name).as_bytes());

        let mut entries: Vec<&EnumEntry> = self.entries.iter().collect();
        entries.sort_by_key(|entry| entry.name().to_string());
        for entry in entries {
            crc_calculator.digest(&entry.fingerprint().as_bytes());
        }

        if let Some(deprecated) = &self.deprecated {
            crc_calculator.digest(format!("{deprecated:?} ").as_bytes());
        }

        crc_calculator.digest(format!("{} ", self.defined_in).as_bytes());

        crc_calculator.digest(&[self.bitmask as u8]);

        crc_calculator.get_crc().into()
    }
}

/// Builder for [`Enum`].
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct EnumBuilder {
    r#enum: Enum,
}

impl Builder for EnumBuilder {
    type Buildable = Enum;

    /// Creates [`Enum`] from builder.
    fn build(&self) -> Enum {
        let mut appears_in = self.r#enum.appears_in.clone();
        for entry in &self.r#enum.entries {
            if !appears_in.contains(&entry.defined_in().to_string()) {
                appears_in.push(entry.defined_in().to_string());
            }
        }

        // We need this to get an error when `Enum` changes
        #[allow(clippy::match_single_binding)]
        match self.r#enum.clone() {
            Enum {
                name,
                description,
                entries,
                bitmask,
                deprecated,
                defined_in,
                ..
            } => Enum {
                name,
                description,
                entries,
                bitmask,
                deprecated,
                defined_in,
                appears_in,
            },
        }
    }
}

impl EnumBuilder {
    /// Creates builder instance.
    ///
    /// Instantiates builder with default values for [`Enum`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets enum name.
    ///
    /// See: [`Enum::name`].
    pub fn set_name(&mut self, name: impl AsRef<str>) -> &mut Self {
        self.r#enum.name = name.as_ref().to_string();
        self
    }

    /// Sets enum description.
    ///
    /// See: [`Enum::description`].
    pub fn set_description(&mut self, description: impl AsRef<str>) -> &mut Self {
        self.r#enum.description = Description::new(description);
        self
    }

    /// Sets collection of enum entries.
    ///
    /// See: [`Enum::entries`].
    pub fn set_entries(&mut self, entries: &[EnumEntry]) -> &mut Self {
        let mut entries = entries.to_vec();
        entries.sort_by_key(|entry| entry.value());

        self.r#enum.entries = entries;
        self
    }

    /// Insert enum entry.
    ///
    /// Replaces an entry with the same [`value`](EnumEntry::value).
    ///
    /// See: [`Enum::entries`].
    pub fn insert_entry(&mut self, entry: EnumEntry) -> &mut Self {
        let value = entry.value();

        let mut entries = vec![];
        let mut inserted = false;
        for existing_entry in self.r#enum.entries.iter() {
            if existing_entry.value() == value {
                entries.push(entry.clone());
                inserted = true;
            } else {
                entries.push(existing_entry.clone());
            }
        }

        if !inserted {
            entries.push(entry);
        }

        self.r#enum.entries = entries;
        self.r#enum.entries.sort_by_key(|entry| entry.value());
        self
    }

    /// Updates entries in place.
    ///
    /// If entries have a conflict in [`EnumEntry::value`], then the latest entry takes priority.
    ///
    /// See: [`Enum::entries`].
    pub fn update_entries<F>(&mut self, filter: F) -> &mut Self
    where
        F: FnMut(&EnumEntry) -> EnumEntry,
    {
        let mut entries = vec![];
        mem::swap(&mut entries, &mut self.r#enum.entries);
        for entry in entries.iter().map(filter) {
            self.insert_entry(entry);
        }
        self
    }

    /// Filters entries in place.
    ///
    /// See: [`Enum::entries`].
    pub fn filter_entries<F>(&mut self, filter: F) -> &mut Self
    where
        F: FnMut(&EnumEntry) -> bool,
    {
        self.r#enum.entries.retain(filter);
        self
    }

    /// Filters entries by names in place.
    ///
    /// If one of the `names` ends with `*`, then it will be considered as a prefix pattern.
    ///
    /// When `exclude` is set to `false`, then only entries that match the provided list of names
    /// will be kept. Otherwise, entries that match the provided list will be excluded.
    ///
    /// See: [`Enum::entries`], [`keep_entries_with_names`](Self::keep_entries_with_names), and
    /// [`remove_entries_with_names`](Self::remove_entries_with_names).
    pub fn filter_entries_by_names(&mut self, names: &[impl AsRef<str>], exclude: bool) -> &mut Self {
        self.r#enum.entries.retain(|entry| {
            for name in names.iter() {
                if name.as_ref().ends_with("*") {
                    if entry
                        .name()
                        .starts_with(name.as_ref().to_string().trim_end_matches('*'))
                    {
                        return !exclude;
                    }
                } else if entry.name() == name.as_ref() {
                    return !exclude;
                }
            }
            exclude
        });
        self
    }

    /// Keep entries matching the provided list of names.
    ///
    /// If one of the `names` ends with `*`, then it will be considered as a prefix pattern.
    ///
    /// See: [`Enum::entries`], [`filter_entries_by_names`](Self::filter_entries_by_names), and
    /// [`remove_entries_with_names`](Self::remove_entries_with_names).
    pub fn keep_entries_with_names(&mut self, names: &[impl AsRef<str>]) -> &mut Self {
        self.filter_entries_by_names(names, false)
    }

    /// Keeps only entries that do not match the provided list of names.
    ///
    /// If one of the `names` ends with `*`, then it will be considered as a prefix pattern.
    ///
    /// See: [`Enum::entries`], [`filter_entries_by_names`](Self::filter_entries_by_names), and
    /// [`keep_entries_with_names`](Self::keep_entries_with_names).
    pub fn remove_entries_with_names(&mut self, names: &[impl AsRef<str>]) -> &mut Self {
        self.filter_entries_by_names(names, true)
    }

    /// Sets whether this enum is a bitmask.
    ///
    /// See: [`Enum::bitmask`].
    pub fn set_bitmask(&mut self, bitmask: bool) -> &mut Self {
        self.r#enum.bitmask = bitmask;
        self
    }

    /// Sets deprecation status.
    ///
    /// See: [`Enum::deprecated`].
    pub fn set_deprecated(&mut self, deprecated: Option<Deprecated>) -> &mut Self {
        self.r#enum.deprecated = deprecated;
        self
    }

    /// Sets canonical name of a dialect in which this enum was defined.
    ///
    /// See: [`Enum::defined_in`] and [`Enum::appears_in`].
    pub fn set_defined_in(&mut self, defined_in: impl AsRef<str>) -> &mut Self {
        let defined_in = defined_in.as_ref().to_string();
        if !self.r#enum.appears_in.contains(&defined_in) {
            self.r#enum.appears_in.push(defined_in.clone());
        }
        self.r#enum.defined_in = defined_in;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::DeprecatedSince;

    #[test]
    fn enum_builder() {
        let entries = vec![EnumEntry::builder().set_name("entry").build()];

        let mav_enum = EnumBuilder::new()
            .set_name("name")
            .set_description("description")
            .set_entries(entries.as_slice())
            .set_bitmask(true)
            .set_deprecated(Some(Deprecated::new(
                DeprecatedSince::default(),
                "better".to_string(),
            )))
            .set_defined_in("unknown")
            .build();

        assert!(matches!(mav_enum, Enum { .. }));
        assert_eq!(mav_enum.name(), "name");
        assert_eq!(mav_enum.description(), "description");
        assert_eq!(mav_enum.entries().len(), 1);
        assert_eq!(mav_enum.get_entry_by_name("entry").unwrap().name(), "entry");
        assert!(mav_enum.bitmask);
        assert_eq!(mav_enum.deprecated().unwrap().replaced_by(), "better");
        assert_eq!(mav_enum.defined_in(), "unknown");
        assert!(matches!(mav_enum.inferred_type(), MavType::UInt8));
    }

    #[test]
    fn entries_are_sorted_by_values() {
        let entries: Vec<EnumEntry> = [
            ("four", 4),
            ("five", 5),
            ("two", 2),
            ("one", 1),
            ("three", 3),
        ]
            .map(|(name, value)| EnumEntry::builder().set_name(name).set_value(value).build())
            .to_vec();

        let mav_enum = EnumBuilder::new()
            .set_name("name".to_string())
            .set_description("description")
            .set_entries(entries.as_slice())
            .set_defined_in("unknown")
            .build();

        assert_eq!(
            mav_enum
                .entries()
                .iter()
                .map(|entry| entry.value())
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        )
    }

    #[test]
    fn enum_entries_filtering() {
        let entries = vec![
            EnumEntry::builder().set_name("entry_a_1").build(),
            EnumEntry::builder().set_name("entry_a_2").build(),
            EnumEntry::builder().set_name("entry_a_b_1").build(),
            EnumEntry::builder().set_name("entry_b_1").build(),
            EnumEntry::builder().set_name("entry_b_2").build(),
        ];

        let mut mav_enum_builder = EnumBuilder::new();
        mav_enum_builder
            .set_name("name")
            .set_description("description")
            .set_entries(entries.as_slice());

        let filtered = mav_enum_builder
            .keep_entries_with_names(&["entry_a_*"])
            .remove_entries_with_names(&["entry_a_b_*"])
            .build();

        assert!(filtered.entries().iter().any(|entry| entry.name() == "entry_a_1"));
        assert!(filtered.entries().iter().any(|entry| entry.name() == "entry_a_2"));
        assert!(!filtered.entries().iter().any(|entry| entry.name() == "entry_a_b_1"));
        assert!(!filtered.entries().iter().any(|entry| entry.name() == "entry_b_1"));
        assert!(!filtered.entries().iter().any(|entry| entry.name() == "entry_b_2"));
    }

    #[test]
    fn enum_entries_update() {
        let entries = vec![
            EnumEntry::builder().set_name("entry_1").set_value(0).build(),
            EnumEntry::builder().set_name("entry_2").set_value(1).build(),
            EnumEntry::builder().set_name("entry_3").set_value(2).build(),
        ];

        let mut mav_enum_builder = EnumBuilder::new();
        mav_enum_builder
            .set_name("name")
            .set_description("description")
            .set_entries(entries.as_slice());

        let updated = mav_enum_builder
            .update_entries(|entry| entry
                .to_builder()
                .set_value(entry.value() + 1)
                .set_description("updated")
                .build()
            )
            .build();

        assert_eq!(updated.get_entry_by_name("entry_1").unwrap().description(), "updated");

        assert_eq!(updated.get_entry_by_name("entry_1").unwrap().value(), 1);
        assert_eq!(updated.get_entry_by_name("entry_2").unwrap().value(), 2);
        assert_eq!(updated.get_entry_by_name("entry_3").unwrap().value(), 3);
    }
}

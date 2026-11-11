use crc_any::CRCu64;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::protocol::{enums::EnumEntryMavCmdFlags, enums::EnumEntryMavCmdParam, Deprecated, Description, Fingerprint};
use crate::utils::{Buildable, Builder};

/// Type alias for [`EnumEntry::value`].
///
/// Currently the container type is set to `u32`.
pub type EnumEntryValue = u32;

/// Enum entry specification.
///
/// Represents enum entry in [`Enum`](super::Enum).
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct EnumEntry {
    value: EnumEntryValue,
    name: String,
    name_stripped: String,
    description: Description,
    cmd_flags: Option<EnumEntryMavCmdFlags>,
    params: Vec<EnumEntryMavCmdParam>,
    wip: bool,
    deprecated: Option<Deprecated>,
    defined_in: String,
}

impl Buildable for EnumEntry {
    type Builder = EnumEntryBuilder;

    /// Creates [`EnumEntryBuilder`] initialised with current enum entry.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use mavinspect::protocol::EnumEntry;
    /// use mavinspect::utils::{Buildable, Builder};
    ///
    /// let original = EnumEntry::builder()
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
    fn to_builder(&self) -> EnumEntryBuilder {
        EnumEntryBuilder {
            entry: self.clone(),
        }
    }
}

impl EnumEntry {
    /// Initiates builder.
    ///
    /// Instead of constructor we use
    /// [builder](https://rust-unofficial.github.io/patterns/patterns/creational/builder.html)
    /// pattern. An instance of [`EnumEntryBuilder`] returned by this function is initialized
    /// with default values. Once desired values are set, you can call [`EnumEntryBuilder::build`] to
    /// obtain [`EnumEntry`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use mavinspect::protocol::EnumEntry;
    /// use mavinspect::utils::Builder;
    ///
    /// let entry = EnumEntry::builder()
    ///     .set_name("name".to_string())
    ///     .set_description("description".to_string())
    ///     .build();
    ///
    /// assert!(matches!(entry, EnumEntry { .. }));
    /// assert_eq!(entry.name(), "name");
    /// assert_eq!(entry.description(), "description");
    /// ```
    pub fn builder() -> EnumEntryBuilder {
        EnumEntryBuilder::new()
    }

    /// Enum entry value.
    pub fn value(&self) -> EnumEntryValue {
        self.value
    }

    /// Enum entry name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Enum entry name without the corresponding enum name prefix.
    ///
    /// Prefix stripping works in the following way: if enum entry starts with enum name, then enum name will be excluded.
    /// For example, if enum name is `SOME_FLAGS` and the enum entry is `SOME_FLAGS_FLAG_A`, then `name_stripped` will
    /// be set to `FLAG_A`. For some languages this allows to create more elegant bindings.
    pub fn name_stripped(&self) -> &str {
        &self.name_stripped
    }

    /// Enum entry description.
    pub fn description(&self) -> &str {
        self.description.as_str()
    }

    /// Enum entry `MAV_CMD` flags.
    pub fn cmd_flags(&self) -> Option<&EnumEntryMavCmdFlags> {
        self.cmd_flags.as_ref()
    }

    /// Enum entry `MAV_CMD` parameters.
    pub fn params(&self) -> &[EnumEntryMavCmdParam] {
        self.params.as_ref()
    }

    /// Work in progress status.
    pub fn wip(&self) -> bool {
        self.wip
    }

    /// Deprecation status.
    pub fn deprecated(&self) -> Option<&Deprecated> {
        self.deprecated.as_ref()
    }

    /// Dialect name where this entry was defined.
    ///
    /// You also can look up for the list of all dialects where enum entries were defined in
    /// [`Enum::appears_in`](super::Enum::appears_in).
    pub fn defined_in(&self) -> &str {
        self.defined_in.as_str()
    }

    /// Enum entry fingerprint.
    ///
    /// A value of opaque type [`Fingerprint`] that contains enum entry CRC.
    pub fn fingerprint(&self) -> Fingerprint {
        let mut crc_calculator = CRCu64::crc64();

        crc_calculator.digest(format!("{:?} ", self.name).as_bytes());
        crc_calculator.digest(&self.value.to_le_bytes());

        if let Some(cmd_flags) = &self.cmd_flags {
            crc_calculator.digest(format!("{cmd_flags:?} ").as_bytes());
        }

        if let Some(deprecated) = &self.deprecated {
            crc_calculator.digest(format!("{deprecated:?} ").as_bytes());
        }

        crc_calculator.digest(format!("{:?} ", self.defined_in).as_bytes());

        crc_calculator.digest(format!("{:?} ", self.params).as_bytes());
        crc_calculator.digest(&[self.wip as u8]);

        crc_calculator.get_crc().into()
    }
}

/// Builder for [`EnumEntry`].
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct EnumEntryBuilder {
    entry: EnumEntry,
}

impl Builder for EnumEntryBuilder {
    type Buildable = EnumEntry;

    /// Creates [`EnumEntry`] from builder.
    fn build(&self) -> EnumEntry {
        // We need this to get error when `EnumEntry` changes
        #[allow(clippy::match_single_binding)]
        match self.entry.clone() {
            EnumEntry {
                value,
                name,
                name_stripped,
                description,
                cmd_flags,
                params,
                wip,
                deprecated,
                defined_in,
            } => EnumEntry {
                value,
                name,
                name_stripped,
                description,
                cmd_flags,
                params,
                wip,
                deprecated,
                defined_in,
            },
        }
    }
}

impl EnumEntryBuilder {
    /// Creates builder instance.
    ///
    /// Instantiates builder with default values for [`EnumEntry`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets enum entry value.
    ///
    /// See: [`EnumEntry::value`].
    pub fn set_value(&mut self, value: EnumEntryValue) -> &mut Self {
        self.entry.value = value;
        self
    }

    /// Sets enum entry name.
    ///
    /// See: [`EnumEntry::name`].
    pub fn set_name(&mut self, name: impl AsRef<str>) -> &mut Self {
        self.entry.name = name.as_ref().to_string();
        self
    }

    /// Sets enum entry name without the corresponding enum name prefix.
    ///
    /// See: [`EnumEntry::name_stripped`].
    pub fn set_name_stripped(&mut self, name_stripped: String) -> &mut Self {
        self.entry.name_stripped = name_stripped;
        self
    }

    /// Sets enum entry description.
    ///
    /// See: [`EnumEntry::description`].
    pub fn set_description(&mut self, description: impl AsRef<str>) -> &mut Self {
        self.entry.description = Description::new(description);
        self
    }

    /// Sets enum entry `MAV_CMD` flags.
    ///
    /// See: [`EnumEntry::cmd_flags`].
    pub fn set_cmd_flags(&mut self, cmd_flags: Option<EnumEntryMavCmdFlags>) -> &mut Self {
        self.entry.cmd_flags = cmd_flags;
        self
    }

    /// Sets enum entry `MAV_CMD` parameters.
    ///
    /// See: [`EnumEntry::params`].
    pub fn set_params(&mut self, params: Vec<EnumEntryMavCmdParam>) -> &mut Self {
        self.entry.params = params;
        self
    }

    /// Sets work in progress status.
    ///
    /// See: [`EnumEntry::wip`].
    pub fn set_wip(&mut self, wip: bool) -> &mut Self {
        self.entry.wip = wip;
        self
    }

    /// Sets deprecation status.
    ///
    /// See: [`EnumEntry::deprecated`].
    pub fn set_deprecated(&mut self, deprecated: Option<Deprecated>) -> &mut Self {
        self.entry.deprecated = deprecated;
        self
    }

    /// Sets dialect name where this entry was defined.
    ///
    /// See: [`EnumEntry::defined_in`].
    pub fn set_defined_in(&mut self, defined_in: impl AsRef<str>) -> &mut Self {
        self.entry.defined_in = defined_in.as_ref().to_string();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{DeprecatedSince, Units};

    #[test]
    fn enum_entry_builder() {
        let entry = EnumEntryBuilder::new()
            .set_value(10)
            .set_name("name")
            .set_description("description")
            .set_cmd_flags(Some(
                EnumEntryMavCmdFlags::builder()
                    .set_has_location(Some(true))
                    .build(),
            ))
            .set_params(vec![EnumEntryMavCmdParam::builder()
                .set_units(Some(Units::AmpereHour))
                .build()])
            .set_wip(true)
            .set_deprecated(Some(Deprecated::new(
                DeprecatedSince::default(),
                "better".to_string(),
            )))
            .set_defined_in("unknown".to_string())
            .build();

        assert!(matches!(entry, EnumEntry { .. }));
        assert_eq!(entry.value(), 10);
        assert_eq!(entry.name(), "name");
        assert_eq!(entry.description(), "description");
        assert!(entry.cmd_flags().unwrap().has_location().unwrap());
        assert_eq!(entry.params().len(), 1);
        assert!(matches!(
            entry.params().get(0).unwrap().units(),
            Some(Units::AmpereHour)
        ));
        assert!(entry.wip());
        assert_eq!(entry.deprecated().unwrap().replaced_by(), "better");
        assert_eq!(entry.defined_in(), "unknown");
    }
}

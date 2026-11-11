//! MAVLink [commands](https://mavlink.io/en/messages/common.html#mav_commands).

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::protocol::{Deprecated, Description, EnumEntry, EnumEntryMavCmdFlags, EnumEntryMavCmdParam};
use crate::utils::Named;

pub(crate) const CMD_ENUM_NAME: &str = "MAV_CMD";

/// [MAV_CMD](https://mavlink.io/en/messages/common.html#mav_commands) enum container type.
pub type MavCmdValue = u16;

/// MAVLink [command](https://mavlink.io/en/messages/common.html#mav_commands).
///
/// # Usage
///
/// Commands can be created only from the corresponding [`EnumEntry`].
///
/// ```rust
/// use mavinspect::protocol::{Command, EnumEntry, EnumEntryMavCmdParam};
/// use mavinspect::utils::{Buildable, Builder};
///
/// let cmd_entry = EnumEntry::builder()
///     .set_name("CMD_CUSTOM")
///     .set_params(vec![
///         EnumEntryMavCmdParam::builder()
///             .set_index(1)
///             .set_description("first")
///             .build(),
///         /* create remaining parameters in the same fashion */
/// #       EnumEntryMavCmdParam::builder().set_index(2).set_description("second").build(),
/// #       EnumEntryMavCmdParam::builder().set_index(3).set_description("third").build(),
/// #       EnumEntryMavCmdParam::builder().set_index(4).set_description("fourth").build(),
/// #       EnumEntryMavCmdParam::builder().set_index(5).set_description("fifth").build(),
/// #       EnumEntryMavCmdParam::builder().set_index(6).set_description("sixth").build(),
/// #       EnumEntryMavCmdParam::builder().set_index(7).set_description("seventh").build(),
///     ])
///     .build();
///
/// let command = Command::from(cmd_entry);
///
/// assert_eq!(
///     command.params().iter().map(|param| param.description()).collect::<Vec<_>>(),
///     vec!["first", "second", "third", "fourth", "fifth", "sixth", "seventh"]
/// );
/// ```
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[cfg_attr(feature = "specta", specta(rename = "MavInspectCommand"))]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Command {
    value: MavCmdValue,
    name: String,
    name_stripped: String,
    description: Description,
    has_location: bool,
    is_destination: bool,
    mission_only: bool,
    params: Vec<EnumEntryMavCmdParam>,
    wip: bool,
    deprecated: Option<Deprecated>,
    defined_in: String,
}

impl Named for Command {
    fn name(&self) -> &str {
        &self.name
    }
}

impl Named for &Command {
    fn name(&self) -> &str {
        &self.name
    }
}

impl From<EnumEntry> for Command {
    /// Constructs [`Command`] from [`EnumEntry`].
    #[inline]
    fn from(value: EnumEntry) -> Self {
        Command::from_enum_entry(&value)
    }
}

impl Command {
    /// Constructs [`Command`] from [`EnumEntry`].
    pub fn from_enum_entry(enum_entry: &EnumEntry) -> Self {
        let cmd_flags = match enum_entry.cmd_flags() {
            None => EnumEntryMavCmdFlags::default(),
            Some(flags) => flags.clone(),
        };

        let params = {
            let mut params = enum_entry.params().to_vec();
            params.sort_by_key(|param| param.index());
            params
        };

        Self {
            value: enum_entry.value() as MavCmdValue,
            name: enum_entry.name().to_string(),
            name_stripped: enum_entry.name_stripped().to_string(),
            description: Description::new(enum_entry.description()),
            has_location: cmd_flags.has_location().unwrap_or_default(),
            is_destination: cmd_flags.is_destination().unwrap_or_default(),
            mission_only: cmd_flags.mission_only().unwrap_or_default(),
            params,
            wip: enum_entry.wip(),
            deprecated: enum_entry.deprecated().cloned(),
            defined_in: enum_entry.defined_in().to_string(),
        }
    }

    /// Enum entry value.
    pub fn value(&self) -> MavCmdValue {
        self.value
    }

    /// Enum entry name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Command name without the corresponding enum name prefix.
    ///
    /// See [`EnumEntry::name_stripped`] for stripping logic details.
    pub fn name_stripped(&self) -> &str {
        &self.name_stripped
    }

    /// Enum entry description.
    pub fn description(&self) -> &str {
        self.description.as_str()
    }

    /// Has location.
    ///
    /// See [`EnumEntryMavCmdFlags::has_location`] for details.
    pub fn has_location(&self) -> bool {
        self.has_location
    }

    /// Is destination.
    ///
    /// See [`EnumEntryMavCmdFlags::is_destination`] for details.
    pub fn is_destination(&self) -> bool {
        self.is_destination
    }

    /// Mission only.
    ///
    /// See [`EnumEntryMavCmdFlags::mission_only`] for details.
    pub fn mission_only(&self) -> bool {
        self.mission_only
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

    /// Dialect name where this command was defined.
    pub fn defined_in(&self) -> &str {
        self.defined_in.as_str()
    }
}

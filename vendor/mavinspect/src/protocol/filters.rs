use crate::protocol::Microservices;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Filter options for [`Dialect`](crate::protocol::Dialect) and [`Protocol`](crate::protocol::Protocol).
///
/// Filters messages, commands, and enums by the following filters:
///
/// * [`Filter::microservices`]: filter by MAVLink [microservices](https://mavlink.io/en/services/). If
///   [`Filter::messages`], [`Filter::enums`], or [`Filter::commands`] are set, then these additional entities will be
///   retained.
/// * [`Filter::messages`]: include only specified MAVLink messages.
/// * [`Filter::enums`]: include only specified MAVLink enums.
/// * [`Filter::commands`]: include only specified MAVLink commands.
///
/// In the case of [`Filter::messages`], [`Filter::enums`], and [`Filter::commands`], if [`Filter::microservices`] is
/// specified, then these entities will be retained in addition to micro-services specification.
///
/// Both [`Filter::messages`], [`Filter::enums`], and [`Filter::commands`] support wildcards in postfix positions. For
/// example, `*` and `MAV_CMD_DO_*` are both valid specifications.
///
/// The decision of which enums and their entries to keep is based on whether enum is referenced in
/// [`MessageField`](crate::protocol::MessageField) or it was referenced by another already included enum in one of the
/// enum parameters ([`EnumEntryMavCmdParam`](crate::protocol::EnumEntryMavCmdParam)).
///
/// Micro-service specifications are similar to filters and have [`Microservices::messages`], [`Microservices::enums`],
/// and [`Microservices::commands`] methods that provide lists of required entities.
///
/// # Usage
///
/// ```rust
/// # use mavinspect::parser::Inspector;
/// # let inspector = Inspector::builder()
/// #     .set_sources(&["./message_definitions/standard"])
/// #     .set_include(&["common"])
/// #     .build().unwrap();
/// # let protocol = inspector.parse().unwrap();
/// use mavinspect::protocol::{Filter, Microservices};
///
/// /* obtain protocol with `common` dialect */
///
/// // Filter
/// let filtered_protocol = protocol.filtered(
///     &Filter::by_microservices(Microservices::HEARTBEAT | Microservices::MISSION)
///     .with_messages(&["FILE_TRANSFER_PROTOCOL"])
///     .with_enums(&["GIMBAL_DEVICE_CAP_FLAGS"])
///     .with_commands(&["MAV_CMD_SET_*"])
/// );
///
/// // Let's inspect `common` dialect
/// let common = filtered_protocol.get_dialect_by_canonical_name("common").unwrap();
///
/// // it should contain `HEARTBEAT` messages
/// assert!(common.contains_message_with_name("HEARTBEAT"));
/// // `FILE_TRANSFER_PROTOCOL` (part of file transfer protocol) was explicitly required by `set_messages`
/// assert!(common.contains_message_with_name("FILE_TRANSFER_PROTOCOL"));
///
/// // but these messages should be filtered out
/// assert!(!common.contains_message_with_name("PROTOCOL_VERSION"));
/// assert!(!common.contains_message_with_name("CAMERA_TRIGGER"));
/// assert!(!common.contains_message_with_name("COMMAND_LONG"));
///
/// // this enum was explicitly requested
/// assert!(common.contains_enum_with_name("GIMBAL_DEVICE_CAP_FLAGS"));
/// // since we have commands, then `MAV_CMD` enum should be present
/// assert!(common.contains_enum_with_name("MAV_CMD"));
/// // and this command was explicitly requested by `MAV_CMD_SET_*` pattern
/// assert!(common.contains_command_with_name("MAV_CMD_SET_MESSAGE_INTERVAL"));
/// // as well as these commands should be present as a part of mission protocol
/// assert!(common.contains_command_with_name("MAV_CMD_DO_SET_MISSION_CURRENT"));
/// assert!(common.contains_command_with_name("MAV_CMD_NAV_WAYPOINT"));
/// // but not these
/// assert!(!common.contains_command_with_name("MAV_CMD_INJECT_FAILURE"));
/// assert!(!common.contains_command_with_name("MAV_CMD_GET_HOME_POSITION"));
/// ```
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Filter {
    messages: Option<Vec<String>>,
    enums: Option<Vec<String>>,
    commands: Option<Vec<String>>,
    microservices: Option<Microservices>,
}

impl Filter {
    /// Default constructor.
    pub fn new() -> Self {
        Self::default()
    }

    /// Constructs [`Filter`] that filter by specified `microservices`.
    pub fn by_microservices(microservices: Microservices) -> Self {
        Self::new().with_microservices(microservices)
    }

    /// Messages to filter.
    pub fn messages(&self) -> Option<&[impl AsRef<str>]> {
        self.messages.as_deref()
    }

    /// Enums to filter.
    pub fn enums(&self) -> Option<&[impl AsRef<str>]> {
        self.enums.as_deref()
    }

    /// Commands to filter.
    pub fn commands(&self) -> Option<&[impl AsRef<str>]> {
        self.commands.as_deref()
    }

    /// Microservices to filter.
    pub fn microservices(&self) -> Option<Microservices> {
        self.microservices
    }

    /// Set [`Self::messages`] filter option.
    pub fn with_messages(mut self, messages: &[impl AsRef<str>]) -> Self {
        self.messages = Some(messages.iter().map(|s| s.as_ref().to_string()).collect());
        self
    }

    /// Set [`Self::enums`] filter option.
    pub fn with_enums(mut self, enums: &[impl ToString]) -> Self {
        self.enums = Some(enums.iter().map(|s| s.to_string()).collect());
        self
    }

    /// Set [`Self::commands`] filter option.
    pub fn with_commands(mut self, commands: &[impl ToString]) -> Self {
        self.commands = Some(commands.iter().map(|s| s.to_string()).collect());
        self
    }

    /// Set [`Self::microservices`] filter option.
    pub fn with_microservices(mut self, microservices: Microservices) -> Self {
        self.microservices = Some(microservices);
        self
    }

    /// Returns `true` if at least one filter is set.
    #[inline]
    pub fn is_some(&self) -> bool {
        self.microservices.is_some()
            || self.messages.is_some()
            || self.enums.is_some()
            || self.commands.is_some()
    }

    /// Returns `true` if no filters are set.
    #[inline]
    pub fn is_none(&self) -> bool {
        !self.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_accept_strings() {
        let filter = Filter::new().with_messages(&["STRING".to_string()]);
        assert_eq!(
            filter.messages().unwrap().first().unwrap().as_ref(),
            "STRING"
        );
    }

    #[test]
    fn filters_accept_strs() {
        let filter = Filter::new().with_messages(&["STRING"]);
        assert_eq!(
            filter.messages().unwrap().first().unwrap().as_ref(),
            "STRING"
        );
    }

    #[test]
    fn filters_convertible_to_strs() {
        let filter = Filter::new().with_messages(&["STRING"]);
        let messages: Vec<&str> = filter
            .messages()
            .unwrap()
            .iter()
            .map(|s| s.as_ref())
            .collect();
        assert_eq!(messages.as_slice(), &["STRING"]);
    }
}

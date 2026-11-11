//! Errors related to XML entities parsing

use std::num::ParseIntError;
use std::str::ParseBoolError;

use thiserror::Error;

use crate::protocol::errors::TypeParseError;
use crate::protocol::errors::{InvalidValueParseError, UnitsError, ValueError};
use crate::protocol::MavType;

/// Errors which may happen during XML definitions discovery
#[derive(Debug, Error)]
pub enum XmlInspectionError {
    /// Two dialect XML definitions with the same canonical names.
    ///
    /// See [`crate::parser::DialectXmlDefinition::canonical_name`].
    #[error("dialects `{first}`, `{second}` have the same canonical name: `{canonical}`")]
    NamingCollision {
        /// First dialect name.
        first: String,
        /// Second dialect name.
        second: String,
        /// Colliding canonical name.
        canonical: String,
    },
    /// Invalid path.
    #[error("invalid path")]
    InvalidPath,
}

/// Errors which may happen during XML parsing
#[derive(Clone, Debug, Error)]
pub enum XmlParseError {
    /// XML tags appears in invalid order.
    #[error("incorrect tags order: tag `{inner}` appears inside tag {outer:?}")]
    TagsNotInOrder {
        /// Outer tag.
        outer: Option<String>,
        /// Inner tag.
        inner: String,
    },
    /// Trying to close a tag which was not immediately opened.
    #[error("incorrect closing tag: opened with `{opened_with:?}` but closed with {closed_with}")]
    UnexpectedClosingTag {
        /// Opening tag.
        opened_with: Option<String>,
        /// Closing tag.
        closed_with: String,
    },

    /// Impossible to parse dialect version.
    #[error("impossible to parse dialect version: {0:?}")]
    VersionParseFailed(ParseIntError),
    /// Impossible to parse dialect ID.
    #[error("impossible to parse dialect ID: {0:?}")]
    DialectIdParseFailed(ParseIntError),

    /// Found enum entry `<entry>` outside of enum definition `<enum>`.
    #[error("found a `<entry>` tag outside a `<enum>` tag")]
    EnumEntryWithoutEnum,
    /// Impossible to parse enum entry value.
    #[error("impossible to parse enum `{enum_name:?}` entry `{entry_name:?}` value '{value:?}': {error:?}"
    )]
    EnumEntryValueParseFailed {
        /// Enum name
        enum_name: String,
        /// Enum entry name
        entry_name: String,
        /// Enum entry value
        value: String,
        /// Parse error
        error: ParseIntError,
    },

    /// Found a `<param>` tag outside a `<enum>` tag.
    #[error("found a `<param>` tag outside a `<enum>` tag")]
    MavCmdParamWithoutEnumEntry,
    /// Invalid `hasLocation` field value in enum entry.
    #[error("invalid `hasLocation` field value: {0:?}")]
    EnumEntryMavCmdFlagsInvalidHasLocation(ParseBoolError),
    /// Invalid `isDestination` field value in enum entry.
    #[error("invalid `isDestination` field value in enum entry: {0:?}")]
    EnumEntryMavCmdFlagsInvalidIsDestination(ParseBoolError),
    /// Invalid `missionOnly` field value in enum entry.
    #[error("invalid `missionOnly` field value in enum entry: {0:?}")]
    EnumEntryMavCmdFlagsInvalidMissionOnly(ParseBoolError),
    /// Invalid `index` field value in enum entry param.
    #[error("invalid `index` field value in enum entry param: {0:?}")]
    EnumEntryMavCmdParamInvalidIndex(ParseIntError),
    /// Invalid `decimalPlaces` field value in enum entry param.
    #[error("invalid `decimalPlaces` field value in enum entry param: {0:?}")]
    EnumEntryMavCmdParamInvalidDecimalPlaces(ParseIntError),
    /// Invalid `increment` field value in enum entry param.
    #[error("invalid `increment` field value in enum entry param: {0:?}")]
    EnumEntryMavCmdParamInvalidIncrement(ValueError),
    /// Invalid `minValue` field value in enum entry param.
    #[error("invalid `minValue` field value in enum entry param: {0:?}")]
    EnumEntryMavCmdParamInvalidMinValue(ValueError),
    /// Invalid `maxValue` field value in enum entry param.
    #[error("invalid `maxValue` field value in enum entry param: {0:?}")]
    EnumEntryMavCmdParamInvalidMaxValue(ValueError),
    /// Invalid `reserved` field value in enum entry param.
    #[error("invalid `reserved` field value in enum entry param: {0:?}")]
    EnumEntryMavCmdParamInvalidReserved(ParseBoolError),
    /// Invalid `default` field value in enum entry param.
    #[error("invalid `default` field value in enum entry param: {0:?}")]
    EnumEntryMavCmdParamInvalidDefaultValue(ValueError),

    /// Invalid message ID.
    #[error("invalid message ID: {0:?}")]
    IncorrectMessageId(ParseIntError),

    /// Impossible to parse message field type
    #[error("invalid message field type: {0:?}")]
    IncorrectMessageFieldType(TypeParseError),
    /// Invalid message field `instance` field.
    #[error("invalid message field `instance` field: {0:?}")]
    IncorrectMessageFieldInstance(ParseBoolError),
    /// Invalid message field `default` field.
    #[error("invalid message field `default` field: {0:?}")]
    IncorrectMessageFieldDefaultValue(ValueError),

    /// Invalid deprecated since date format.
    #[error("invalid deprecated since date format: {0}")]
    DeprecatedSinceDateIncorrectFormat(String),
    /// Invalid deprecated since year or month.
    #[error("invalid deprecated since year: {0}")]
    DeprecatedSinceIncorrectYear(ParseIntError),
    /// Invalid deprecated since year or month.
    #[error("invalid deprecated since month: {0}")]
    DeprecatedSinceIncorrectMonth(ParseIntError),

    /// Invalid value
    #[error("invalid value")]
    IncorrectInvalidValue(InvalidValueParseError),

    /// Invalid units
    #[error("invalid units: {0:?}")]
    IncorrectUnits(UnitsError),

    /// Found message field `<field>` tag outside of message definition `<message>`.
    #[error("found `<field>` tag outside of `<message>` tag")]
    MessageFieldWithoutMessage,
    /// Found `<extensions>` tag outside of message definition `<message>`.
    #[error("found `<extensions>` tag outside of `<message>` tag")]
    MessageExtensionsWithoutMessage,

    /// Found `<description>` tag outside of any appropriate tag.
    #[error("found `<description>` tag outside of any appropriate tag")]
    DescriptionWithoutSubject,
    /// Found `<deprecated>` tag outside of any appropriate tag.
    #[error("found `<deprecated>` tag outside of any appropriate tag")]
    DeprecationWithoutSubject,
    /// Found `<wip>` tag outside of any appropriate tag.
    #[error("found `<wip>` tag outside of any appropriate tag")]
    WipWithoutSubject,

    /// Message field is too small to contain enum.
    #[error(
        "message field `{field_name}` type `{field_type:?}` is too small \
    to contain enum `{enum_name}` of type {enum_type:?}"
    )]
    MessageFieldISTooSmallForEnum {
        /// Name of the enum.
        enum_name: String,
        /// Inferred type of the enum.
        enum_type: MavType,
        /// Name of the message field.
        field_name: String,
        /// Message field type.
        field_type: MavType,
    },

    /// Generic error.
    #[error("error: {0}")]
    Other(String),
}

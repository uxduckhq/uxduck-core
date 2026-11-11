//! Protocol errors.

use std::num::{ParseFloatError, ParseIntError};

use thiserror::Error;

/// Errors which may happen during protocol parsing.
#[derive(Debug, Clone, Error)]
pub enum UnitsError {
    /// Invalid units. See: [`crate::protocol::Units`].
    #[error("incorrect units: {0}")]
    Other(String),
}

/// Value parse errors.
///
/// See: [`crate::protocol::Value`].
#[derive(Debug, Clone, Error)]
pub enum ValueError {
    /// Value has incorrect integer value.
    ///
    /// See: [`crate::protocol::Value`].
    #[error("incorrect integer value: {0:?}")]
    ParseIntError(ParseIntError),
    /// Value has incorrect floating point value.
    ///
    /// See: [`crate::protocol::Value`].
    #[error("incorrect floating point value: {0:?}")]
    ParseFloatError(ParseFloatError),
}

/// Errors during parsing invalid value specification.
///
/// See: [`crate::protocol::MessageFieldInvalidValue`].
#[derive(Debug, Clone, Error)]
pub enum InvalidValueParseError {
    /// Incorrect input for invalid field value specification.
    ///
    /// See: [`crate::protocol::MessageFieldInvalidValue`].
    #[error("incorrect message field invalid value specification: {0}")]
    IncorrectSpecification(String),
    /// Impossible to parse value that specified for invalid field value.
    ///
    /// See: [`crate::protocol::MessageFieldInvalidValue`], [`crate::protocol::Value`].
    #[error("message field invalid value specification parsing error: {0:?}")]
    IncorrectValue(String, ValueError),
    /// Generic error.
    #[error("error: {0:?}")]
    Other(String),
}

/// Type parse errors.
///
/// See: [`crate::protocol::MavType`].
#[derive(Debug, Clone, Error)]
pub enum TypeParseError {
    /// Field type has an invalid array length specification.
    ///
    /// See: [`crate::protocol::MavType`].
    #[error("field type has an invalid array specification: {0}")]
    ArrayTypeLengthError(String, ParseIntError),
    /// Arrays of arrays are not allowed.
    ///
    /// See: [`crate::protocol::MavType`].
    #[error("arrays of arrays are not allowed: {0}")]
    NestedArraysAreNotSupportedError(String),
    /// Invalid type specification.
    ///
    /// See: [`crate::protocol::MavType`].
    #[error("invalid type specification: {0}")]
    Other(String),
}

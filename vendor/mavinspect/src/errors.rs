//! # MAVInspect errors

use crate::parser::errors::{XmlInspectionError, XmlParseError};
use crate::protocol::errors::{InvalidValueParseError, TypeParseError, UnitsError, ValueError};

/// Generic result type returned by MAVInspect functions and methods.
pub type Result<T> = core::result::Result<T, Error>;

/// Wraps all errors that may happen during MAVInspect life cycle.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Error during XML definitions inspection.
    #[error("inspection error: {0:?}")]
    Inspection(#[from] XmlInspectionError),
    /// Error during XML parsing.
    #[error("XML parse error: {0:?}")]
    Parse(#[from] XmlParseError),
    /// Error during parsing units.
    #[error("incorrect units: {0:?}")]
    Units(#[from] UnitsError),
    /// Error during parsing value specification.
    #[error("incorrect value specs: {0:?}")]
    Value(#[from] ValueError),
    /// Error during parsing invalid value specification.
    #[error("incorrect invalid value specs: {0:?}")]
    InvalidValue(#[from] InvalidValueParseError),
    /// Error during parsing type specification.
    #[error("incorrect type specs: {0:?}")]
    Type(#[from] TypeParseError),
    /// IO error.
    #[error("IO error: {0:?}")]
    Io(#[from] std::io::Error),
}

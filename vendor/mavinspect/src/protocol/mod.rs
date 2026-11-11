//! MAVLink protocol entities.
//!
//! The key entities are:
//!
//! * [`Protocol`] — contains dialects.
//! * [`Dialect`] — represents a particular MAVLink dialect.
//! * [`Message`] — MAVLink message.
//! * [`Enum`] — MAVLink enum.
//! * [`Command`] — MAVLink command.
//!
//! Builders for entities which utilise [builder](https://rust-unofficial.github.io/patterns/patterns/creational/builder.html)
//! pattern can be found in the [`builders`] module.
#![warn(missing_docs)]

#[allow(clippy::module_inception)]
mod protocol;
#[doc(inline)]
pub use protocol::Protocol;

mod dialect;
#[doc(inline)]
pub use dialect::Dialect;

mod enums;
#[doc(inline)]
pub use enums::{Enum, EnumEntry, EnumEntryMavCmdFlags, EnumEntryMavCmdParam, EnumEntryValue};

mod messages;
#[doc(inline)]
pub use messages::{Message, MessageField, MessageFieldInvalidValue, MessageId};

mod common;
#[doc(inline)]
pub use common::deprecated::{Deprecated, DeprecatedSince};
#[doc(inline)]
pub use common::description::Description;
#[doc(inline)]
pub use common::fingerprints::Fingerprint;
#[doc(inline)]
pub use common::mav_type::MavType;
#[doc(inline)]
pub use common::units::Units;
#[doc(inline)]
pub use common::value::Value;
#[doc(inline)]
pub use common::{DialectId, DialectVersion};

pub mod commands;
#[doc(inline)]
pub use commands::Command;

mod microservices;
#[doc(inline)]
pub use microservices::Microservices;

mod filters;
#[doc(inline)]
pub use filters::Filter;

pub mod errors;

pub mod metadata;
#[doc(inline)]
pub use metadata::{DialectMetadata, DialectTags};

/// Builders.
///
/// Builders available for [`protocol`](self) entities.
///
/// See: [`Builder`](crate::utils::Builder).
pub mod builders {
    pub use super::dialect::DialectBuilder;
    pub use super::enums::{
        EnumBuilder, EnumEntryBuilder, EnumEntryMavCmdFlagsBuilder, EnumEntryMavCmdParamBuilder,
    };
    pub use super::messages::{MessageBuilder, MessageFieldBuilder};
}

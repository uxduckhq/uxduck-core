//! MAVLink parsing utilities.

mod inspector;
pub use inspector::{Inspector, InspectorBuilder};

mod definition;
pub use definition::DialectXmlDefinition;

pub mod errors;

mod metadata;
pub use metadata::MavInspectMetadata;

mod xml;

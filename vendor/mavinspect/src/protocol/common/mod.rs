pub mod deprecated;
pub mod fingerprints;
pub mod mav_type;
pub mod units;
pub mod value;
pub mod description;

/// Type used to contain `dialect` identifier specified in MAVLink [XML definitions](https://mavlink.io/en/guide/xml_schema.html).  
pub type DialectId = u32;

/// Type used to contain minor dialect `version` specified in MAVLink [XML definitions](https://mavlink.io/en/guide/xml_schema.html).
///
/// Dialect version appears in some messages like [HEARTBEAT](https://mavlink.io/en/messages/common.html#HEARTBEAT). In
/// such cases it is usually not directly set by user.
pub type DialectVersion = u8;

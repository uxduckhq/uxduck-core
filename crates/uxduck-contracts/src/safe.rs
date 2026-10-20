//! 64-bit counters and microsecond timestamps cross the wire as JSON numbers
//! that JavaScript holds exactly (ADR-0001: "safe integers or decimal
//! strings, never lossy numbers"). Their schema says so, which keeps
//! generated TypeScript on `number` rather than `bigint`.

use schemars::{Schema, SchemaGenerator, json_schema};

/// 2^53 - 1: the largest integer a JavaScript number holds exactly.
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// `#[schemars(schema_with = "crate::safe::integer")]` on a `u64`.
#[must_use]
pub fn integer(_: &mut SchemaGenerator) -> Schema {
    json_schema!({ "type": "integer", "minimum": 0, "maximum": MAX_SAFE_INTEGER })
}

/// The same on an `Option<u64>` that may be omitted or null.
#[must_use]
pub fn optional_integer(_: &mut SchemaGenerator) -> Schema {
    json_schema!({ "type": ["integer", "null"], "minimum": 0, "maximum": MAX_SAFE_INTEGER })
}

/// A bounded build commit, without arbitrary names or diagnostic text.
#[must_use]
pub fn optional_commit(_: &mut SchemaGenerator) -> Schema {
    json_schema!({ "type": ["string", "null"], "pattern": "^[0-9a-f]{7,40}$" })
}

/// The same on an `i64`, such as a JSON-RPC id.
#[must_use]
pub fn signed_integer(_: &mut SchemaGenerator) -> Schema {
    #[allow(clippy::cast_possible_wrap, reason = "MAX_SAFE_INTEGER fits in i64")]
    let bound = MAX_SAFE_INTEGER as i64;
    json_schema!({ "type": "integer", "minimum": -bound, "maximum": bound })
}

/// The same on an `Option<i64>` that may be omitted or null.
#[must_use]
pub fn optional_signed_integer(_: &mut SchemaGenerator) -> Schema {
    #[allow(clippy::cast_possible_wrap, reason = "MAX_SAFE_INTEGER fits in i64")]
    let bound = MAX_SAFE_INTEGER as i64;
    json_schema!({ "type": ["integer", "null"], "minimum": -bound, "maximum": bound })
}

/// Any JSON value. schemars writes `true` for `serde_json::Value`, which the
/// TypeScript generator drops, and the generated validator then strips the
/// field; an empty schema object is kept as `unknown`.
#[must_use]
pub fn any(_: &mut SchemaGenerator) -> Schema {
    json_schema!({})
}

/// The same on an `Option<Value>`.
#[must_use]
pub fn optional_any(_: &mut SchemaGenerator) -> Schema {
    json_schema!({})
}

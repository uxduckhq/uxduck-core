//! Protocol and compatibility versions, and the release stage.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The JSON-RPC protocol version. `initialize` is frozen at `v1`.
pub const PROTOCOL_VERSION: u32 = 1;

/// The oldest client version this daemon accepts.
pub const MIN_CLIENT_VERSION: &str = "0.1.0";

/// What a release may be used for, on the wire in `initialize` and
/// `daemon/about`. UXDuck now flies real vehicles and always says `flight`;
/// the field stays because an older interface held real vehicles on the
/// ground unless it read `flight`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ReleaseStage {
    /// Only an older UXDuck says this: it flew simulated vehicles and armed
    /// a real one only for a ground test.
    GroundTesting,
    /// Nothing is held back.
    Flight,
}

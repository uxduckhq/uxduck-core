#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// MAVLink entity deprecation specs.
///
/// Applicable to [`Enum`](crate::protocol::Enum), [`EnumEntry`](crate::protocol::EnumEntry), and
/// [`Message`](crate::protocol::Message).
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[cfg_attr(feature = "specta", specta(rename = "MavInspectDeprecated"))]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Deprecated {
    since: DeprecatedSince,
    replaced_by: String,
}

impl Deprecated {
    /// Default constructor
    ///
    /// # Arguments
    ///
    /// * `since` - since when deprecation is in effect.
    /// * `replaced_by` - which entry replaces this deprecated enum entry.
    pub fn new(since: DeprecatedSince, replaced_by: String) -> Self {
        Self { since, replaced_by }
    }

    /// Returns since when deprecation is in effect.
    pub fn since(&self) -> &DeprecatedSince {
        &self.since
    }

    /// Returns the name of the enum which replaces the current one.
    pub fn replaced_by(&self) -> &str {
        &self.replaced_by
    }
}

/// Specifies when enum entry was deprecated.
///
/// Used in [`Deprecated`].
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct DeprecatedSince {
    year: i32,
    month: u8,
}

impl DeprecatedSince {
    /// Default constructor.
    ///
    /// # Arguments
    ///
    /// * `year` - year as signed integer.
    /// * `month` - month as unsigned integer.
    pub fn new(year: i32, month: u8) -> Self {
        Self { year, month }
    }

    /// Year of deprecation.
    pub fn year(&self) -> i32 {
        self.year
    }

    /// Month of deprecation.
    pub fn month(&self) -> u8 {
        self.month
    }
}

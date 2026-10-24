//! A vehicle's parameters (ADR-0004, ADR-0005). The autopilot holds the
//! truth: the daemon keeps the last set it downloaded, per vehicle and per
//! boot, and a write is a compare-and-swap on the value it last confirmed.

use serde::{Deserialize, Serialize};

/// `MAV_PARAM_TYPE`: how the autopilot stores a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ParamKind {
    Uint8,
    Int8,
    Uint16,
    Int16,
    Uint32,
    Int32,
    Uint64,
    Int64,
    Real32,
    Real64,
}

impl ParamKind {
    const ALL: [Self; 10] = [
        Self::Uint8,
        Self::Int8,
        Self::Uint16,
        Self::Int16,
        Self::Uint32,
        Self::Int32,
        Self::Uint64,
        Self::Int64,
        Self::Real32,
        Self::Real64,
    ];

    /// From `MAV_PARAM_TYPE` (1 to 10); anything else is taken as a float.
    #[must_use]
    pub fn from_mav(value: u8) -> Self {
        usize::from(value)
            .checked_sub(1)
            .and_then(|index| Self::ALL.get(index).copied())
            .unwrap_or(Self::Real32)
    }

    #[must_use]
    pub fn to_mav(self) -> u8 {
        Self::ALL
            .iter()
            .position(|kind| *kind == self)
            .and_then(|index| u8::try_from(index + 1).ok())
            .unwrap_or(9)
    }

    #[must_use]
    pub const fn is_integer(self) -> bool {
        !matches!(self, Self::Real32 | Self::Real64)
    }
}

/// One parameter of one vehicle. Its id is `<vehicle id>/<NAME>`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Parameter {
    pub vehicle_id: String,
    pub name: String,
    pub value: f64,
    pub kind: ParamKind,
    /// The firmware's default, when the vehicle said it (by MAVLink FTP,
    /// S74), as of the boot it was downloaded in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<f64>,
}

impl Parameter {
    /// Changed from its default, when the default is known.
    #[must_use]
    pub fn changed(&self) -> bool {
        self.default.is_some_and(|d| !same_value(self.value, d))
    }

    #[must_use]
    pub fn id(&self) -> String {
        parameter_id(&self.vehicle_id, &self.name)
    }
}

#[must_use]
pub fn parameter_id(vehicle_id: &str, name: &str) -> String {
    format!("{vehicle_id}/{name}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransferStatus {
    /// Nothing downloaded yet.
    Unknown,
    Downloading,
    Uploading,
    /// Matches the vehicle as of its current boot.
    Synced,
    Failed,
}

/// Where a vehicle's parameter set stands. Its id is the vehicle id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParameterSet {
    pub vehicle_id: String,
    pub status: TransferStatus,
    /// Why it failed, in words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub count: u32,
    /// When the autopilot booted, in the daemon's clock: the set belongs
    /// to this boot, and a different one means it must be downloaded again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_instant_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced_at_us: Option<u64>,
    /// How far a download has come while it runs (S127).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<TransferProgress>,
}

/// How far a transfer has come (S127, ADR-0029 P8): what is done of the
/// total, which try this is (above 1 when the vehicle went quiet and it
/// asked again), and when it last moved, so a stall shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferProgress {
    pub done: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u32>,
    pub attempt: u8,
    /// When it last moved on, in the daemon's clock.
    pub at_us: u64,
    /// Up to the vehicle, rather than down from it.
    #[serde(default)]
    pub upload: bool,
}

/// Two parameter values are the same if they round to the same `f32`, the
/// width the protocol carries.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::float_cmp,
    reason = "comparing at the protocol's own precision"
)]
pub fn same_value(a: f64, b: f64) -> bool {
    a as f32 == b as f32
}

/// A parameter value as people read it: the shortest number that is the
/// same `f32`, so 0.3499999940395355 reads 0.35.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    reason = "showing at the protocol's own precision"
)]
pub fn show_value(value: f64) -> String {
    (value as f32).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_show_at_the_protocols_precision() {
        assert_eq!(show_value(f64::from(0.35_f32)), "0.35");
        assert_eq!(show_value(20.0), "20");
        assert_eq!(show_value(-1.0), "-1");
        assert_eq!(show_value(0.517_545_461_654_663_1), "0.51754546");
    }

    #[test]
    fn kinds_map_to_mav_param_type_and_back() {
        assert_eq!(ParamKind::from_mav(9), ParamKind::Real32);
        assert_eq!(ParamKind::from_mav(6), ParamKind::Int32);
        assert_eq!(ParamKind::from_mav(0), ParamKind::Real32);
        for value in 1..=10 {
            assert_eq!(ParamKind::from_mav(value).to_mav(), value);
        }
        assert!(same_value(0.1, f64::from(0.1f32)));
        assert!(!same_value(0.1, 0.2));
    }
}

/// A named, pinned parameter set for a vehicle (C7): "Approved
/// 2026-10-01", who set it and why. The vehicle's parameters are compared
/// with it; it syncs with the account so every computer sees the same.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParamBaseline {
    pub id: String,
    pub vehicle_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// The operator who set it, by name at the time.
    pub set_by: String,
    /// Their verified account, when signed in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Every parameter's value when it was set, by name.
    pub values: std::collections::BTreeMap<String, f64>,
    pub created_at_us: u64,
    pub version: u64,
}

/// The longest baseline name or note, in characters.
pub const MAX_BASELINE_TEXT: usize = 200;

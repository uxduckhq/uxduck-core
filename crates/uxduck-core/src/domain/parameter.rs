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

    /// The values a whole-number type holds, from its lowest to one past
    /// its highest, so a 64-bit type's top (2^64 or 2^63, which an f64
    /// holds exactly) is refused; None for a real.
    #[must_use]
    pub const fn range(self) -> Option<(f64, f64)> {
        match self {
            Self::Uint8 => Some((0.0, 256.0)),
            Self::Int8 => Some((-128.0, 128.0)),
            Self::Uint16 => Some((0.0, 65_536.0)),
            Self::Int16 => Some((-32_768.0, 32_768.0)),
            Self::Uint32 => Some((0.0, 4_294_967_296.0)),
            Self::Int32 => Some((-2_147_483_648.0, 2_147_483_648.0)),
            Self::Uint64 => Some((0.0, 18_446_744_073_709_551_616.0)),
            Self::Int64 => Some((-9_223_372_036_854_775_808.0, 9_223_372_036_854_775_808.0)),
            Self::Real32 | Self::Real64 => None,
        }
    }

    /// Why the type cannot hold a value, as what it takes: a whole number
    /// in its range, or any finite number for a real. None when it can.
    /// An autopilot clamps or wraps a value its type cannot hold, so a
    /// parameter no definitions describe is still written as it is kept.
    #[must_use]
    pub fn refuses(self, value: f64) -> Option<String> {
        if !value.is_finite() {
            return Some("takes a number".into());
        }
        let (low, end) = self.range()?;
        if value.fract() != 0.0 {
            return Some("takes a whole number".into());
        }
        (value < low || value >= end).then(|| match self {
            Self::Uint64 => "takes a whole number of 0 or more, less than 2^64".into(),
            Self::Int64 => "takes a whole number of -2^63 or more, less than 2^63".into(),
            _ => format!("takes a whole number from {low} to {}", end - 1.0),
        })
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

/// A parameter value as a `.param` file keeps it: a value the protocol
/// carries as an `f32` as the shortest number that reads back as that
/// `f32`, as Mission Planner writes it (0.3, not 0.30000001192092896), and
/// any other value in full.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::float_cmp,
    reason = "telling an f32 value from a wider one"
)]
pub fn file_value(value: f64) -> String {
    if f64::from(value as f32) == value {
        show_value(value)
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_whole_number_type_refuses_what_it_cannot_hold() {
        assert_eq!(ParamKind::Int8.refuses(127.0), None);
        assert_eq!(ParamKind::Int8.refuses(-128.0), None);
        assert_eq!(
            ParamKind::Int8.refuses(300.0).as_deref(),
            Some("takes a whole number from -128 to 127")
        );
        assert_eq!(
            ParamKind::Uint8.refuses(-1.0).as_deref(),
            Some("takes a whole number from 0 to 255")
        );
        assert_eq!(
            ParamKind::Int16.refuses(40_000.0).as_deref(),
            Some("takes a whole number from -32768 to 32767")
        );
        assert_eq!(ParamKind::Int32.refuses(2_147_483_647.0), None);
        assert_eq!(
            ParamKind::Int32.refuses(1.5).as_deref(),
            Some("takes a whole number")
        );
        assert_eq!(
            ParamKind::Uint64.refuses(-2.0).as_deref(),
            Some("takes a whole number of 0 or more, less than 2^64")
        );
        assert_eq!(ParamKind::Real32.refuses(1e30), None);
        assert_eq!(
            ParamKind::Real32.refuses(f64::NAN).as_deref(),
            Some("takes a number")
        );
    }

    #[test]
    fn a_64_bit_whole_number_refuses_what_it_cannot_hold() {
        let two_64 = 18_446_744_073_709_551_616.0;
        let two_63 = 9_223_372_036_854_775_808.0;
        let unsigned = Some("takes a whole number of 0 or more, less than 2^64");
        let signed = Some("takes a whole number of -2^63 or more, less than 2^63");
        assert_eq!(ParamKind::Uint64.refuses(1e100).as_deref(), unsigned);
        assert_eq!(ParamKind::Uint64.refuses(two_64).as_deref(), unsigned);
        assert_eq!(ParamKind::Uint64.refuses(two_64 - 2048.0), None);
        assert_eq!(ParamKind::Uint64.refuses(0.0), None);
        assert_eq!(ParamKind::Int64.refuses(1e100).as_deref(), signed);
        assert_eq!(ParamKind::Int64.refuses(two_63).as_deref(), signed);
        assert_eq!(ParamKind::Int64.refuses(two_63 - 1024.0), None);
        assert_eq!(ParamKind::Int64.refuses(-two_63), None);
        assert_eq!(
            ParamKind::Int64.refuses(-two_63 - 4096.0).as_deref(),
            signed
        );
        assert_eq!(
            ParamKind::Int64.refuses(0.5).as_deref(),
            Some("takes a whole number")
        );
    }

    #[test]
    fn values_show_at_the_protocols_precision() {
        assert_eq!(show_value(f64::from(0.35_f32)), "0.35");
        assert_eq!(show_value(20.0), "20");
        assert_eq!(show_value(-1.0), "-1");
        assert_eq!(show_value(0.517_545_461_654_663_1), "0.51754546");
    }

    #[test]
    #[allow(clippy::cast_possible_truncation, reason = "the protocol's float")]
    fn a_file_value_reads_back_as_the_same_value() {
        assert_eq!(file_value(f64::from(0.3_f32)), "0.3");
        assert_eq!(file_value(0.3), "0.3");
        assert_eq!(file_value(1500.0), "1500");
        assert_eq!(file_value(16_777_217.0), "16777217");
        assert_eq!(file_value(f64::from(u32::MAX)), "4294967295");
        // Every f32 sign, exponent and a spread of mantissas, read as a
        // .param file is read: as an f64, then to the protocol's f32.
        for bits in (0..=u32::MAX).step_by(65_537) {
            let value = f32::from_bits(bits);
            if !value.is_finite() {
                continue;
            }
            let read: f64 = file_value(f64::from(value)).parse().unwrap();
            assert_eq!((read as f32).to_bits(), value.to_bits(), "{value:e}");
        }
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
/// 2026-10-01", who set it and why. Retired on 2026-10-24 (maintainer):
/// kept only so journals that have one still read.
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

/// How a saved parameter set came to be (S217, ADR-0037 V19).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SavedParamsOrigin {
    /// Saved from the vehicle's values with Save these.
    Saved,
    /// A `.param` file imported, kept before anything was written.
    Imported,
    /// The vehicle's values saved before another set was applied.
    Before,
    /// The vehicle's values saved before a firmware install (V10).
    Firmware,
    /// A tuning snapshot (ADR-0054 T5): only its tuning parameters.
    Tuning,
}

/// A saved parameter set (S217, ADR-0037 V19): all of a vehicle's
/// parameters exactly as read, under a name, kept on this computer as a
/// `.param` file in the vehicle's Files and never synced to the account.
/// Only ever saved, applied, renamed or deleted, never edited.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedParams {
    pub id: String,
    pub vehicle_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    pub origin: SavedParamsOrigin,
    /// Who saved it, by name at the time.
    pub saved_by: String,
    pub saved_at_us: u64,
    /// The vehicle's firmware when it was saved; absent for an import.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
    /// Every parameter's value, by name.
    pub values: std::collections::BTreeMap<String, f64>,
    /// Its file's name in "Vehicle files › Parameters", and the transfer
    /// that lists it there; absent when this UXDuck keeps no files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<String>,
    pub version: u64,
}

/// The longest saved set's name or note, in characters.
pub const MAX_SAVED_PARAMS_TEXT: usize = 200;

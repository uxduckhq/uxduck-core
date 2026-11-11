#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::errors::Result;
use crate::protocol::errors::ValueError;
use crate::protocol::MavType;

/// Value specification.
///
/// Each value corresponds to a specific [`MavType`].
///
/// Used in [`MessageField`](crate::protocol::MessageField),
/// [`EnumEntryMavCmdParam`](crate::protocol::EnumEntryMavCmdParam), and
/// [`MessageFieldInvalidValue`](crate::protocol::MessageFieldInvalidValue).
///
/// See: [message](https://mavlink.io/en/guide/xml_schema.html#messages) section in MAVLink XML schema documentation.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[cfg_attr(feature = "specta", specta(rename = "MavInspectValue"))]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Value {
    /// 8-bit signed int [`i8`].
    Int8(i8),
    /// 16-bit signed int [`i16`].
    Int16(i16),
    /// 32-bit signed int [`i32`].
    Int32(i32),
    /// 64-bit signed int [`i64`].
    Int64(i64),
    /// 8-bit unsigned int [`u8`].
    UInt8(u8),
    /// 16-bit unsigned int [`u16`].
    UInt16(u16),
    /// 32-bit unsigned int [`u16`].
    UInt32(u32),
    /// 64-bit unsigned int [`u64`].
    UInt64(u64),
    /// 32-bit single precision floating pointer [`f32`].
    Float(f32),
    /// 64-bit single precision floating pointer [`f64`].
    Double(f64),
    /// Character, a 8-bit unsigned int [`u8`].
    Char(u8),
    /// Maximum value of the specified [`MavType`].
    Max(MavType),
}

impl Value {
    /// Parses value from string for specified [`MavType`].
    ///
    /// Currently it dos not supports arrays.
    pub fn parse(s: &str, r#type: &MavType) -> Result<Self> {
        let s = s.to_uppercase();

        // Max value
        if s.ends_with("_MAX") {
            return Ok(Value::Max(r#type.clone()));
        }

        Ok(match r#type {
            MavType::Int8 => Self::Int8(s.parse::<i8>().map_err(ValueError::ParseIntError)?),
            MavType::Int16 => Self::Int16(s.parse::<i16>().map_err(ValueError::ParseIntError)?),
            MavType::Int32 => Self::Int32(s.parse::<i32>().map_err(ValueError::ParseIntError)?),
            MavType::Int64 => Self::Int64(s.parse::<i64>().map_err(ValueError::ParseIntError)?),
            MavType::UInt8 => Self::UInt8(s.parse::<u8>().map_err(ValueError::ParseIntError)?),
            MavType::UInt16 => Self::UInt16(s.parse::<u16>().map_err(ValueError::ParseIntError)?),
            MavType::UInt32 => Self::UInt32(s.parse::<u32>().map_err(ValueError::ParseIntError)?),
            MavType::UInt64 => Self::UInt64(s.parse::<u64>().map_err(ValueError::ParseIntError)?),
            MavType::Float => {
                if s == "NAN" {
                    Self::Float(f32::NAN)
                } else {
                    Self::Float(s.parse::<f32>().map_err(ValueError::ParseFloatError)?)
                }
            }
            MavType::Double => {
                if s == "NAN" {
                    Self::Double(f64::NAN)
                } else {
                    Self::Double(s.parse::<f64>().map_err(ValueError::ParseFloatError)?)
                }
            }
            MavType::Char => Self::UInt8(s.parse::<u8>().map_err(ValueError::ParseIntError)?),
            MavType::UInt8MavlinkVersion => {
                Self::UInt8(s.parse::<u8>().map_err(ValueError::ParseIntError)?)
            }
            MavType::Array(mav_type, _) => Self::parse(s.as_str(), mav_type)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nan_values() {
        let parsed = Value::parse("NaN", &MavType::Float);

        assert!(parsed.is_ok());
        assert!(matches!(parsed, Ok(Value::Float(_))));

        if let Ok(Value::Float(val)) = parsed {
            assert!(val.is_nan());
        } else {
            panic!("expected value should be float NaN, given: {:?}", parsed)
        }
    }
}

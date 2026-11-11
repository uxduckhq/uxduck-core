use std::cmp::Ordering;
use std::ops::Deref;
use std::str::FromStr;

use regex::Regex;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::errors::Result;
use crate::protocol::errors::TypeParseError;

const RE_ARRAY_FIELD_TYPE: &str = r"^(.+)\[(\d+)\]$";

/// Types of message fields.
///
/// Optional parameters designate array size for array types.
///
/// Similar to a field in a C struct - the size of the data required to store/represent the data type.
/// Fields can be signed/unsigned integers of size 8, 16, 23, 64 bits, single/double precision
/// [IEEE754](https://en.wikipedia.org/wiki/IEEE_754) floating point numbers. They can also be arrays of these scalar
/// types.
///
/// > **Note!** We've intentionally excluded outdated `array[*]` type as it is not used in modern message definitions.
/// > See MAVLink message definitions
/// > [XML schema](https://github.com/ArduPilot/pymavlink/blob/master/generator/mavschema.xsd).
///
/// Defines type of a [`MessageField`](crate::protocol::MessageField) and [`Value`](crate::protocol::Value).
///
/// # Examples
///
/// ## Construct
///
/// Create a field type from string:
///
/// ```rust
/// use mavinspect::protocol::MavType;
///
/// assert!(matches!(
///     "int8_t".parse::<MavType>().unwrap(),
///     MavType::Int8
/// ));
///
/// ```
///
/// Alternatively, use [`MavType::parse`]:
///
/// ```rust
/// use mavinspect::protocol::MavType;
///
/// assert!(matches!(
///     MavType::parse("double").unwrap(),
///     MavType::Double
/// ));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum MavType {
    /// Unsigned integer: [`u8`]
    #[default]
    UInt8,
    /// Unsigned integer: [`u16`]
    UInt16,
    /// Unsigned integer: [`u32`]
    UInt32,
    /// Unsigned integer: [`u64`]
    UInt64,
    /// Signed integer: [`i8`]
    Int8,
    /// Signed integer: [`i16`]
    Int16,
    /// Signed integer: [`i32`]
    Int32,
    /// Signed integer: [`i64`]
    Int64,
    /// Float with single precision: [`f32`]
    Float,
    /// Float with double precision: [`f64`]
    Double,
    /// Char (byte)
    Char,
    /// Special type for MAVLink version
    UInt8MavlinkVersion,
    /// Array of any scalar type
    Array(Box<MavType>, usize),
}

impl PartialOrd for MavType {
    /// Compares sizes of base types.
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.base_type()
            .size()
            .partial_cmp(&other.base_type().size())
    }
}

impl FromStr for MavType {
    type Err = crate::errors::Error;

    fn from_str(s: &str) -> Result<Self> {
        MavType::parse(s)
    }
}

impl MavType {
    /// Parses field type from string.
    ///
    /// **Note** that [`MavType`] also implements [`FromStr`] with same functionality.
    ///
    /// # Arguments
    ///
    /// * `s` — string representation of the message type
    ///
    /// # Errors
    ///
    /// Returns [`TypeParseError`] if message can't be parsed.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use mavinspect::protocol::MavType;
    ///
    /// assert!(matches!(
    ///     MavType::parse("uint64_t").unwrap(),
    ///     MavType::UInt64
    /// ));
    /// ```
    pub fn parse(s: &str) -> Result<MavType> {
        let s = s.trim();
        let re_array_field_type = Regex::new(RE_ARRAY_FIELD_TYPE)
            .expect("MAVInspect bug: Array regular expression MUST be correct");

        let mav_type = match s {
            "int8_t" => MavType::Int8,
            "int16_t" => MavType::Int16,
            "int32_t" => MavType::Int32,
            "int64_t" => MavType::Int64,
            "uint8_t" => MavType::UInt8,
            "uint16_t" => MavType::UInt16,
            "uint32_t" => MavType::UInt32,
            "uint64_t" => MavType::UInt64,
            "float" => MavType::Float,
            "double" => MavType::Double,
            "char" => MavType::Char,
            "uint8_t_mavlink_version" => MavType::UInt8MavlinkVersion,
            _ if re_array_field_type.is_match(s) => {
                let (_, [base_type_name, vec_length]) = re_array_field_type
                    .captures(s)
                    .map(|c| c.extract())
                    .unwrap();
                let vec_length = vec_length
                    .parse::<usize>()
                    .map_err(|err| TypeParseError::ArrayTypeLengthError(s.to_string(), err))?;

                if re_array_field_type.is_match(base_type_name) {
                    return Err(
                        TypeParseError::NestedArraysAreNotSupportedError(s.to_string()).into(),
                    );
                }

                Self::Array(Box::new(Self::parse(base_type_name)?), vec_length)
            }
            &_ => return Err(TypeParseError::Other(s.to_string()).into()),
        };

        Ok(mav_type)
    }

    /// Type name as in XML definition.
    ///
    /// Returns original type name.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use mavinspect::protocol::MavType;
    ///
    /// for expected in ["int8_t", "float", "double[4]"] {
    ///     let mav_type = MavType::parse(expected).unwrap();
    ///     let actual = mav_type.definition_name();
    ///     assert_eq!(actual, expected.to_string());
    /// }
    /// ```
    pub fn definition_name(&self) -> String {
        match self {
            MavType::Int8 => "int8_t".to_string(),
            MavType::Int16 => "int16_t".to_string(),
            MavType::Int32 => "int32_t".to_string(),
            MavType::Int64 => "int64_t".to_string(),
            MavType::UInt8 => "uint8_t".to_string(),
            MavType::UInt16 => "uint16_t".to_string(),
            MavType::UInt32 => "uint32_t".to_string(),
            MavType::UInt64 => "uint64_t".to_string(),
            MavType::Float => "float".to_string(),
            MavType::Double => "double".to_string(),
            MavType::Char => "char".to_string(),
            MavType::UInt8MavlinkVersion => "uint8_t_mavlink_version".to_string(),
            MavType::Array(mav_type, length) => format!("{}[{length}]", mav_type.definition_name()),
        }
    }

    /// Types name as a C-type.
    ///
    /// Type as they required for [`extra_crc`](crate::protocol::Message::crc_extra) calculation.
    ///
    /// Returns original type name.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use mavinspect::protocol::MavType;
    ///
    /// for expected in ["int8_t", "float", "double[4]"] {
    ///     let mav_type = MavType::parse(expected).unwrap();
    ///     let actual = mav_type.c_type();
    ///     assert_eq!(actual, expected);
    /// }
    ///
    /// // `uint8_t_mavlink_version` has to be represented as `uint8_t` instead of its original name.
    /// assert_eq!(
    ///     MavType::parse("uint8_t_mavlink_version").unwrap().c_type(),
    ///     "uint8_t".to_string()
    /// );
    /// ```
    pub fn c_type(&self) -> String {
        self.definition_name()
            .replace("uint8_t_mavlink_version", "uint8_t")
    }

    /// Returns a corresponding Rust type.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use mavinspect::protocol::MavType;
    ///
    /// assert_eq!(MavType::Int8.rust_type(), "i8");
    /// assert_eq!(MavType::UInt32.rust_type(), "u32");
    /// assert_eq!(MavType::Float.rust_type(), "f32");
    /// assert_eq!(MavType::Array(Box::new(MavType::Char), 4).rust_type(), "[u8; 4]");
    /// ```
    pub fn rust_type(&self) -> String {
        match self {
            MavType::Int8 => "i8".to_string(),
            MavType::Int16 => "i16".to_string(),
            MavType::Int32 => "i32".to_string(),
            MavType::Int64 => "i64".to_string(),
            MavType::UInt8 => "u8".to_string(),
            MavType::UInt16 => "u16".to_string(),
            MavType::UInt32 => "u32".to_string(),
            MavType::UInt64 => "u64".to_string(),
            MavType::Float => "f32".to_string(),
            MavType::Double => "f64".to_string(),
            MavType::Char => "u8".to_string(),
            MavType::UInt8MavlinkVersion => "u8".to_string(),
            MavType::Array(mav_type, length) => format!("[{}; {length}]", mav_type.rust_type()),
        }
    }

    /// Calculates MavLink type size.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use mavinspect::protocol::MavType;
    ///
    /// assert_eq!(MavType::Int8.size(), 1);
    /// assert_eq!(MavType::UInt32.size(), 4);
    /// assert_eq!(MavType::Float.size(), 4);
    /// assert_eq!(MavType::Array(Box::new(MavType::Char), 4).size(), 4);
    /// ```
    pub fn size(&self) -> usize {
        match self {
            MavType::Int8 => 1,
            MavType::Int16 => 2,
            MavType::Int32 => 4,
            MavType::Int64 => 8,
            MavType::UInt8 => 1,
            MavType::UInt16 => 2,
            MavType::UInt32 => 4,
            MavType::UInt64 => 8,
            MavType::Float => 4,
            MavType::Double => 8,
            MavType::Char => 1,
            MavType::UInt8MavlinkVersion => 1,
            MavType::Array(mav_type, length) => mav_type.size() * length,
        }
    }

    /// Whether this type is array.
    pub fn is_array(&self) -> bool {
        matches!(self, MavType::Array(_, _))
    }

    /// Provides base type.
    ///
    /// For scalars this is the same type as originals. For arrays only first level is checked.
    pub fn base_type(&self) -> &Self {
        match self {
            MavType::Array(mav_type, _) => mav_type.deref(),
            _ => self,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;

    #[test]
    fn list_field_type_patten_is_correct() {
        let re = Regex::new(RE_ARRAY_FIELD_TYPE).unwrap();

        assert!(re.is_match("char[123]"));
        assert!(re.is_match("float[123]"));

        let (_, [type_name, vec_length]) = re.captures("char[123]").map(|c| c.extract()).unwrap();

        assert_eq!(type_name, "char");
        assert_eq!(vec_length, "123");
    }

    #[test]
    fn definition_types_are_reversible() {
        for expected in [
            "int8_t",
            "uint32_t",
            "float",
            "double",
            "uint8_t_mavlink_version",
            "int64_t[5]",
            "uint16_t[5]",
            "float[2]",
            "double[4]",
        ] {
            let mav_type = MavType::parse(expected).unwrap();
            let actual = mav_type.definition_name();
            assert_eq!(actual, expected.to_string());
        }
    }

    #[test]
    fn ordering() {
        assert!(MavType::UInt8 < MavType::UInt16);
        assert!(MavType::Int8 < MavType::UInt16);
        assert!(MavType::Int16 < MavType::Float);
        assert!(MavType::Float < MavType::Double);
        assert!(MavType::Array(Box::new(MavType::UInt8), 10) < MavType::UInt16);
    }
}
